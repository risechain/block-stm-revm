//! Blazingly fast Parallel EVM in Rust.

// TODO: Better types & API for third-party integration

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use revm::primitives::{AccountInfo, Address, U256};

// We take the last 8 bytes of an address as its hash. This
// seems fine as the addresses themselves are hash suffixes,
// and precomiles' suffix should be unique, too.
// TODO: Make sure this is acceptable for production
#[derive(Default)]
struct AddressHasher(u64);
impl Hasher for AddressHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut suffix = [0u8; 8];
        suffix.copy_from_slice(&bytes[bytes.len() - 8..]);
        self.0 = u64::from_be_bytes(suffix);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
type BuildAddressHasher = BuildHasherDefault<AddressHasher>;

// TODO: More granularity here to separate an account's balance, nonce, and
// code instead of marking conflict at the whole account. That way checking
// for a mid-block code change (contract deployment) doesn't need to traverse
// a potential long list of lazy balance updates, etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemoryLocation {
    Basic(Address),
    Storage(Address, U256),
}

// We only need the full memory location to read from storage.
// We then identify the locations with its hash in the multi-version
// data, write and read sets, which is much faster than rehashing
// on every single lookup & validation.
type MemoryLocationHash = u64;

// This is primarily used for memory location hash, but can also be used for
// transaction indexes, etc.
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn write_u64(&mut self, id: u64) {
        self.0 = id;
    }
    fn write_usize(&mut self, id: usize) {
        self.0 = id as u64;
    }
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
}
type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

// TODO: It would be nice if we could tie the different cases of
// memory locations & values at the type level, to prevent lots of
// matches & potentially dangerous mismatch mistakes.
#[derive(Debug, Clone)]
enum MemoryValue {
    Basic(Box<AccountInfo>),
    // We lazily update the beneficiary balance to avoid continuous
    // dependencies as all transactions read and write to it. We also
    // lazy update the recipient balance of raw transfers, which is also
    // common (popular CEX addresses, etc).
    // We fully evaluate these account states at the end of the block or
    // when there is an explicit read.
    LazyBalanceAddition(U256),
    Storage(U256),
}

enum MemoryEntry {
    Data(TxIncarnation, MemoryValue),
    // When an incarnation is aborted due to a validation failure, the
    // entries in the multi-version data structure corresponding to its
    // write set are replaced with this special ESTIMATE marker.
    // This signifies that the next incarnation is estimated to write to
    // the same memory locations. An incarnation stops and is immediately
    // aborted whenever it reads a value marked as an ESTIMATE written by
    // a lower transaction, instead of potentially wasting a full execution
    // and aborting during validation.
    // The ESTIMATE markers that are not overwritten are removed by the next
    // incarnation.
    Estimate,
}

// The index of the transaction in the block.
// TODO: Consider downsizing to [u32].
type TxIdx = usize;

// The i-th time a transaction is re-executed, counting from 0.
// TODO: Consider downsizing to [u32].
type TxIncarnation = usize;

// - ReadyToExecute(i) --try_incarnate--> Executing(i)
// Non-blocked execution:
//   - Executing(i) --finish_execution--> Executed(i)
//   - Executed(i) --finish_validation--> Validated(i)
//   - Executed/Validated(i) --try_validation_abort--> Aborting(i)
//   - Aborted(i) --finish_validation(w.aborted=true)--> ReadyToExecute(i+1)
// Blocked execution:
//   - Executing(i) --add_dependency--> Aborting(i)
//   - Aborting(i) --resume--> ReadyToExecute(i+1)
#[derive(PartialEq, Debug)]
enum IncarnationStatus {
    ReadyToExecute,
    Executing,
    Executed,
    Validated,
    Aborting,
}

#[derive(PartialEq, Debug)]
struct TxStatus {
    incarnation: TxIncarnation,
    status: IncarnationStatus,
}

// TODO: Clearer doc. See `Scheduler` in `scheduler.rs` for now.
type TransactionsStatus = Vec<TxStatus>;
// TODO: Consider using [SmallVec] for these `[DepsList]`
type TransactionsDependents = Vec<Vec<TxIdx>>;
type TransactionsDependenciesNum = Vec<usize>;

// We maintain an in-memory multi-version data structure that stores for
// each memory location the latest value written per transaction, along
// with the associated transaction incarnation. When a transaction reads
// a memory location, it obtains from the multi-version data structure the
// value written to this location by the highest transaction that appears
// before it in the block, along with the associated version. If no previous
// transactions have written to a location, the value would be read from the
// storage state before block execution.
#[derive(Clone, Debug, PartialEq)]
struct TxVersion {
    tx_idx: TxIdx,
    tx_incarnation: TxIncarnation,
}

// The origin of a memory read. It could be from the live multi-version
// data structure or from storage (chain state before block execution).
#[derive(Debug, PartialEq)]
enum ReadOrigin {
    MvMemory(TxVersion),
    Storage,
}

// For validation: a list of read origins (previous transaction versions)
// for each read memory location.
type ReadLocations = HashMap<MemoryLocationHash, Vec<ReadOrigin>, BuildIdentityHasher>;

// The memory locations needed to execute an incarnation.
// TODO: Implement a [Default] that pre-allocate two slots for each
// container, which are the [from] and [to] accounts of the transaction.
#[derive(Default)]
struct ReadSet {
    locations: ReadLocations,
    // Execution cache to determine if an account was changed.
    accounts: HashMap<MemoryLocationHash, AccountInfo, BuildIdentityHasher>,
}

/// Errors when reading a memory location.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadError {
    /// Cannot read memory location from storage.
    StorageError(String),
    /// Memory location not found.
    NotFound,
    /// This memory location has been written by a lower transaction.
    BlockingIndex(TxIdx),
    /// There has been an inconsistent read like reading the same
    /// location from storage in the first call but from [VmMemory] in
    /// the next.
    InconsistentRead,
    /// The stored memory value type doesn't match its location type.
    /// TODO: Handle this at the type level?
    InvalidMemoryLocationType,
}

// The updates made by this transaction incarnation, which is applied
// to the multi-version data structure at the end of execution.
type WriteSet = Vec<(MemoryLocationHash, MemoryValue)>;

// A scheduled worker task
// TODO: Add more useful work when there are idle workers like near
// the end of block execution, while waiting for a huge blocking
// transaction to resolve, etc.
#[derive(Debug)]
enum Task {
    Execution(TxVersion),
    Validation(TxVersion),
}

// This optimization is desired as we constantly index into many
// vectors of the block-size size. It can yield up to 5% improvement.
macro_rules! index_mutex {
    ($vec:expr, $index:expr) => {
        // SAFETY: A correct scheduler would not leak indexes larger
        // than the block size, which is the size of all vectors we
        // index via this macro. Otherwise, DO NOT USE!
        // TODO: Better error handling for the mutex.
        unsafe { $vec.get_unchecked($index).lock().unwrap() }
    };
}

mod pevm;
pub use pevm::{execute, execute_revm, execute_revm_sequential, PevmError, PevmResult};
mod mv_memory;
mod primitives;
pub use primitives::get_block_spec;
mod scheduler;
mod storage;
pub use storage::{AccountBasic, EvmAccount, InMemoryStorage, RpcStorage, Storage, StorageWrapper};
mod vm;
pub use vm::{ExecutionError, PevmTxExecutionResult};
