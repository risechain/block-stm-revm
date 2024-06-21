//! Blazingly fast Parallel EVM for EVM.

// TODO: Better types & API please

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use ahash::AHashMap;

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

// TODO: More granularity here, for instance, to separate an account's
// balance, nonce, etc. instead of marking conflict at the whole account.
// That way we may also generalize beneficiary balance's lazy update
// behaviour into `MemoryValue` for more use cases.
// TODO: It would be nice if we could tie the different cases of
// memory locations & values at the type level, to prevent lots of
// matches & potentially dangerous mismatch mistakes.
// TODO: Confirm that we're not missing anything, like bytecode.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemoryLocation {
    Basic(Address),
    Storage(Address, U256),
}

// No more hashing is required as we already identify memory locations by
// their hash in the multi-version data structure, read & write sets. [dashmap]
// having a dedicated interface for this use case (that skips hashing for `u64`
// keys) would make our code cleaner and "faster". Nevertheless, the compiler
// should be good enough to optimize these cases anyway.
#[derive(Default)]
struct IdentityHasher(MemoryLocationHash);
impl Hasher for IdentityHasher {
    fn write_u64(&mut self, hash: MemoryLocationHash) {
        self.0 = hash;
    }
    fn finish(&self) -> MemoryLocationHash {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
}
type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

// We only need the full memory location to read from storage.
// We then identify the locations with its hash in the multi-version
// data, write and read sets, which is much faster than rehashing
// on every single lookup & validation.
type MemoryLocationHash = u64;

#[derive(Debug, Clone)]
enum MemoryValue {
    Basic(Box<AccountInfo>),
    // We lazily update the beneficiary balance to avoid continuous
    // dependencies as all transactions read and write to it. We
    // either evaluate all these beneficiary account states at the
    // end of BlockSTM, or when there is an explicit read.
    // Important: The value of this lazy (update) balance is the gas
    // it receives in the transaction, to be added to the absolute
    // balance at the end of the previous transaction.
    // We can probably generalize this to `AtomicBalanceAddition`.
    LazyBalanceAddition(U256),
    Storage(U256),
}

enum MemoryEntry {
    Data(TxIncarnation, MemoryValue),
    // When an incarnation is aborted due to a validation failure, the
    // entries in the multi-version data structure corresponding to its
    // write set are replaced with this special ESTIMATE marker.
    // This signifies that the next incarnation is estimated to write to the
    // same memory locations. An incarnation stops and is immediately aborted
    // whenever it reads a value marked as an ESTIMATE written by a lower
    // transaction, instead of potentially wasting a full execution and aborting
    // during validation.
    // The ESTIMATE markers that are not overwritten are removed by the next
    // incarnation.
    Estimate,
}

// The index of the transaction in the block.
type TxIdx = usize;

// The i-th time a transaction is re-executed, counting from 0.
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
// We use `Vec` for dependents to simplify runtime update code.
// We use `HashMap` for dependencies as we're only adding
// them during preprocessing and removing them during processing.
// The underlying `HashSet` is to simplify index deduplication logic
// while adding new dependencies.
// TODO: Intuitively both should share a similar data structure?
type TransactionsDependents = Vec<Vec<TxIdx>>;
type TransactionsDependencies = AHashMap<TxIdx, Vec<TxIdx>>;

// BlockSTM maintains an in-memory multi-version data structure that
// stores for each memory location the latest value written per
// transaction, along with the associated transaction version. When a
// transaction reads a memory location, it obtains from the
// multi-version data structure the value written to this location by
// the highest transaction that appears before it in the block, along
// with the associated version. For instance, tx5 would read the value
// written by tx3 even when tx6 has also written to it. If no previous
// transactions have written to a location, the value would be read
// from the storage state before block execution.
#[derive(Clone, Debug, PartialEq)]
struct TxVersion {
    tx_idx: TxIdx,
    tx_incarnation: TxIncarnation,
}

// The origin of a memory read. It could be from the live multi-version
// data structure or from storage (chain state before block execution).
#[derive(Debug, PartialEq)]
enum ReadOrigin {
    // The previous transaction version that wrote the value.
    MvMemory(TxVersion),
    Storage,
}

// For validation: a list of read origins (previous transaction versions)
// for each read memory location.
type ReadLocations = HashMap<MemoryLocationHash, Vec<ReadOrigin>, BuildIdentityHasher>;

/// Errors when reading a memory location while executing BlockSTM.
/// TODO: Better name & elaboration
#[derive(Debug, Clone, PartialEq)]
pub enum ReadError {
    /// Cannot read memory location from storage.
    StorageError(String),
    /// Memory location not found.
    NotFound,
    /// This memory location has been written by a lower transaction.
    BlockingIndex(usize),
    /// The stored memory value type doesn't match its location type.
    /// TODO: Handle this at the type level?
    InvalidMemoryLocationType,
}

// The memory locations needed to execute an incarnation.
// While a hash map is cleaner and reduce duplication chances,
// vectors are noticeably faster in the mainnet benchmark.
// TODO: Implement a [Default] that pre-allocate two slots for each
// array, which are the [from] and [to] accounts of the transaction.
#[derive(Default)]
struct ReadSet {
    locations: ReadLocations,
    // Execution cache to determine if an account was changed.
    // TODO: Better organize the type to seprate what is needed
    // for execution only, and what is needed for validation.
    // TODO: We can use [MemoryLocationHash] here!
    accounts: HashMap<Address, AccountInfo, BuildAddressHasher>,
}

// The updates made by this transaction incarnation, which is applied
// to the multi-version data structure at the end of execution.
type WriteSet = Vec<(MemoryLocationHash, MemoryValue)>;

// TODO: Properly type this
type ExecutionTask = TxVersion;

// TODO: Properly type this
type ValidationTask = TxVersion;

// TODO: Properly type this
#[derive(Debug)]
enum Task {
    Execution(ExecutionTask),
    Validation(ValidationTask),
}

// This optimization is desired as we constantly index into many
// vectors of the block-size size. It can yield up to 5% improvement.
macro_rules! index_mutex {
    ( $vec:expr, $index:expr) => {
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
pub use storage::{AccountBasic, EvmAccount, InMemoryStorage, RpcStorage, Storage};
mod vm;
pub use vm::{ExecutionError, PevmTxExecutionResult};
