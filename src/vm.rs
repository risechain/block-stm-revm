use std::sync::atomic::{AtomicU8, Ordering};

use ahash::AHashMap;
use alloy_chains::Chain;
use alloy_rpc_types::Receipt;
use defer_drop::DeferDrop;
use revm::{
    primitives::{
        AccountInfo, Address, BlockEnv, Bytecode, CfgEnv, EVMError, Env, ExecutionResult,
        InvalidTransaction, ResultAndState, SpecId, TransactTo, TxEnv, B256, U256,
    },
    Context, Database, Evm, EvmContext, Handler,
};

use crate::{
    mv_memory::MvMemory, EvmAccount, MemoryEntry, MemoryLocation, MemoryLocationHash, MemoryValue,
    ReadError, ReadLocations, ReadOrigin, ReadSet, Storage, TxIdx, TxVersion, WriteSet,
};

/// The execution error from the underlying EVM executor.
// Will there be DB errors outside of read?
pub type ExecutionError = EVMError<ReadError>;

/// Represents the state transitions of the EVM accounts after execution.
/// If the value is [None], it indicates that the account is marked for removal.
/// If the value is [Some(new_state)], it indicates that the account has become [new_state].
type EvmStateTransitions = AHashMap<Address, Option<EvmAccount>>;

// Different chains may have varying reward policies.
// This enum specifies which policy to follow, with optional
// pre-calculated data to assist in reward calculations.
enum RewardPolicy {
    Ethereum,
}

/// Execution result of a transaction
#[derive(Debug, Clone, PartialEq)]
pub struct PevmTxExecutionResult {
    /// Receipt of execution
    // TODO: Consider promoting to [ReceiptEnvelope] if there is high demand
    pub receipt: Receipt,
    /// State that got updated
    pub state: EvmStateTransitions,
}

impl PevmTxExecutionResult {
    /// Construct a Pevm execution result from a raw Revm result.
    /// Note that [cumulative_gas_used] is preset to the gas used of this transaction.
    /// It should be post-processed with the remaining transactions in the block.
    pub fn from_revm(spec_id: SpecId, ResultAndState { result, state }: ResultAndState) -> Self {
        Self {
            receipt: Receipt {
                status: result.is_success().into(),
                cumulative_gas_used: result.gas_used() as u128,
                logs: result.into_logs(),
            },
            state: state
                .into_iter()
                .filter(|(_, account)| account.is_touched())
                .map(|(address, account)| {
                    if account.is_selfdestructed()
                    // https://github.com/ethereum/EIPs/blob/96523ef4d76ca440f73f0403ddb5c9cb3b24dcae/EIPS/eip-161.md
                    || account.is_empty() && spec_id.is_enabled_in(SpecId::SPURIOUS_DRAGON)
                    {
                        (address, None)
                    } else {
                        (address, Some(EvmAccount::from(account)))
                    }
                })
                .collect(),
        }
    }
}

pub(crate) enum VmExecutionResult {
    ReadError {
        blocking_tx_idx: TxIdx,
    },
    ExecutionError(ExecutionError),
    Ok {
        execution_result: PevmTxExecutionResult,
        read_locations: ReadLocations,
        write_set: WriteSet,
        // From which transaction index do we need to validate from after
        // this execution. This is [None] when no validation is required.
        // For instance, for transactions that only read and write to the
        // from and to addresses, which preprocessing & lazy evaluation has
        // already covered. Note that this is used to set the min validation
        // index in the scheduler, meaing a `None` here will still be validated
        // if there was a lower transaction that has broken the preprocessed
        // dependency chain and returned [Some].
        // TODO: Better name & doc
        next_validation_idx: Option<TxIdx>,
    },
}

// A database interface that intercepts reads while executing a specific
// transaction with Revm. It provides values from the multi-version data
// structure & storage, and tracks the read set of the current execution.
// TODO: Simplify this type, like grouping [from] and [to] into a
// [preprocessed_addresses] or a [preprocessed_locations] vector.
struct VmDb<'a, S: Storage> {
    vm: &'a Vm<'a, S>,
    tx_idx: &'a TxIdx,
    from: &'a Address,
    from_hash: MemoryLocationHash,
    to: Option<&'a Address>,
    to_hash: Option<MemoryLocationHash>,
    is_maybe_lazy: bool,
    read_set: ReadSet,
    // Check if this transaction has read anything other than its sender
    // and to accounts. We must validate from this transaction if it has.
    only_read_from_and_to: bool,
}

impl<'a, S: Storage> VmDb<'a, S> {
    fn new(
        vm: &'a Vm<'a, S>,
        tx_idx: &'a TxIdx,
        from: &'a Address,
        from_hash: MemoryLocationHash,
        to: Option<&'a Address>,
        to_hash: Option<MemoryLocationHash>,
        is_maybe_lazy: bool,
    ) -> Self {
        Self {
            vm,
            tx_idx,
            from,
            from_hash,
            to,
            to_hash,
            is_maybe_lazy,
            only_read_from_and_to: true,
            read_set: ReadSet::default(),
        }
    }

    fn get_address_hash(&self, address: &Address) -> MemoryLocationHash {
        if address == self.from {
            self.from_hash
        } else if Some(address) == self.to {
            self.to_hash.unwrap()
        } else {
            self.vm.get_address_hash(address)
        }
    }
}

impl<'a, S: Storage> Database for VmDb<'a, S> {
    type Error = ReadError;

    // TODO: More granularity here to ensure we only record dependencies for,
    // say, only an account's balance instead of the whole account info.
    fn basic(
        &mut self,
        address: Address,
        // TODO: Better way for REVM to notify explicit reads
        is_preload: bool,
    ) -> Result<Option<AccountInfo>, Self::Error> {
        // We only return full accounts on explicit usage.
        if is_preload {
            return Ok(None);
        }

        // We return a mock for a non-contract recipient to avoid unncessarily
        // evaluating its balance here. Also skip transactions with the same from
        // & to until we have lazy updates for the sender nonce & balance.
        if self.is_maybe_lazy
            && Some(&address) == self.to
            // TODO: Live check (i.e., from [MvMemory] not [Storage]) for a
            // contract deployed then used in the same block with non-data!!
            && !self.vm.storage.is_contract(&address).unwrap()
        {
            return Ok(Some(AccountInfo {
                // We need this hack to not flag this an empty account for
                // destruction. TODO: A cleaner solution here.
                nonce: 1,
                ..AccountInfo::default()
            }));
        }

        if &address != self.from && Some(&address) != self.to {
            self.only_read_from_and_to = false;
        }

        let location_hash = self.get_address_hash(&address);
        let read_origins = self.read_set.locations.entry(location_hash).or_default();
        // For some reasons REVM may call to the same location several time!
        // We can return caches here but early benchmarks show it's not worth
        // it. Clearing the origins for now.
        read_origins.clear();

        let mut final_account = None;
        let mut balance_addition = U256::ZERO;

        // Try reading from multi-verion data
        if self.tx_idx > &0 {
            // We enforce consecutive indexes for locations that all transactions write to like
            // the beneficiary balance. The goal is to not wastefully evaluate when we know
            // we're missing data -- let's just depend on the missing data instead.
            let need_consecutive_idxs = location_hash == self.vm.beneficiary_location_hash;
            // While we can depend on the precise missing transaction index (known during lazy evaluation),
            // through benchmark constantly retrying via the previous transaction index performs much better.
            let reschedule = Err(ReadError::BlockingIndex(self.tx_idx - 1));

            if let Some(written_transactions) = self.vm.mv_memory.read_location(&location_hash) {
                let mut current_idx = self.tx_idx;
                let mut iter = written_transactions.range(..current_idx);

                // Fully evaluate lazy updates
                loop {
                    match iter.next_back() {
                        Some((blocking_idx, MemoryEntry::Estimate)) => {
                            return if need_consecutive_idxs {
                                reschedule
                            } else {
                                Err(ReadError::BlockingIndex(*blocking_idx))
                            }
                        }
                        Some((closest_idx, MemoryEntry::Data(tx_incarnation, value))) => {
                            if need_consecutive_idxs && closest_idx != &(current_idx - 1) {
                                return reschedule;
                            }
                            read_origins.push(ReadOrigin::MvMemory(TxVersion {
                                tx_idx: *closest_idx,
                                tx_incarnation: *tx_incarnation,
                            }));
                            match value {
                                MemoryValue::Basic(account) => {
                                    let mut info = *account.clone();
                                    info.balance += balance_addition;
                                    final_account = Some(info);
                                    break;
                                }
                                MemoryValue::LazyBalanceAddition(addition) => {
                                    balance_addition += addition;
                                    current_idx = closest_idx;
                                }
                                _ => return Err(ReadError::InvalidMemoryLocationType),
                            }
                        }
                        _ => {
                            if need_consecutive_idxs && current_idx > &0 {
                                return reschedule;
                            }
                            break;
                        }
                    }
                }
            } else if need_consecutive_idxs {
                return reschedule;
            }
        }

        // Fall back to storage
        if final_account.is_none() {
            read_origins.push(ReadOrigin::Storage);
            final_account = match self.vm.storage.basic(&address) {
                Ok(Some(account)) => {
                    let mut info = AccountInfo::from(account);
                    info.balance += balance_addition;
                    Some(info)
                }
                Ok(None) => {
                    if balance_addition > U256::ZERO {
                        Some(AccountInfo::from_balance(balance_addition))
                    } else {
                        None
                    }
                }
                Err(err) => return Err(ReadError::StorageError(format!("{err:?}"))),
            };
        }

        // Register read accounts to check if they have changed (been written to)
        if let Some(account) = &final_account {
            self.read_set.accounts.insert(
                location_hash,
                AccountInfo {
                    // Avoid cloning the code as we can compare its hash
                    code: None,
                    ..*account
                },
            );
        }

        Ok(final_account)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.vm
            .storage
            .code_by_hash(&code_hash)
            .map(|code| code.map(Bytecode::from).unwrap_or_default())
            .map_err(|err| ReadError::StorageError(format!("{err:?}")))
    }

    fn has_storage(&mut self, address: Address) -> Result<bool, Self::Error> {
        self.vm
            .storage
            .has_storage(&address)
            .map_err(|err| ReadError::StorageError(format!("{err:?}")))
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.only_read_from_and_to = false;

        let location_hash = self
            .vm
            .hasher
            .hash_one(MemoryLocation::Storage(address, index));

        let read_origins = self.read_set.locations.entry(location_hash).or_default();
        // For some reasons REVM may call to the same location several time!
        // We can return caches here but early benchmarks show it's not worth
        // it. Clearing the origins for now.
        read_origins.clear();

        // Try reading from multi-verion data
        if self.tx_idx > &0 {
            if let Some(written_transactions) = self.vm.mv_memory.read_location(&location_hash) {
                if let Some((closest_idx, entry)) =
                    written_transactions.range(..self.tx_idx).next_back()
                {
                    match entry {
                        MemoryEntry::Data(tx_incarnation, MemoryValue::Storage(value)) => {
                            read_origins.push(ReadOrigin::MvMemory(TxVersion {
                                tx_idx: *closest_idx,
                                tx_incarnation: *tx_incarnation,
                            }));
                            return Ok(*value);
                        }
                        MemoryEntry::Estimate => {
                            return Err(ReadError::BlockingIndex(*closest_idx))
                        }
                        _ => return Err(ReadError::InvalidMemoryLocationType),
                    }
                }
            }
        }

        // Fall back to storage
        read_origins.push(ReadOrigin::Storage);
        self.vm
            .storage
            .storage(&address, &index)
            .map_err(|err| ReadError::StorageError(format!("{err:?}")))
    }

    fn block_hash(&mut self, number: U256) -> Result<B256, Self::Error> {
        self.vm
            .storage
            .block_hash(&number)
            .map_err(|err| ReadError::StorageError(format!("{err:?}")))
    }
}

pub(crate) struct Vm<'a, S: Storage> {
    hasher: &'a ahash::RandomState,
    storage: &'a S,
    mv_memory: &'a MvMemory,
    chain: Chain,
    spec_id: SpecId,
    block_env: BlockEnv,
    beneficiary_location_hash: MemoryLocationHash,
    reward_policy: RewardPolicy,
    // TODO: Make REVM [Evm] or at least [Handle] thread safe to consume
    // the [TxEnv] into them here, to avoid heavy re-initialization when
    // re-executing a transaction.
    txs: DeferDrop<Vec<TxEnv>>,
    // There are fatal errors that we should retry on like lacking funds
    // to pay fees (a previous transaction funding the account via internal
    // tx hasn't completed, etc.), reverting before registering the full
    // read set for correct validaiton, etc.
    retried_tx: Vec<AtomicU8>,
}

impl<'a, S: Storage> Vm<'a, S> {
    pub(crate) fn new(
        hasher: &'a ahash::RandomState,
        storage: &'a S,
        mv_memory: &'a MvMemory,
        chain: Chain,
        spec_id: SpecId,
        block_env: BlockEnv,
        txs: Vec<TxEnv>,
    ) -> Self {
        Self {
            hasher,
            storage,
            mv_memory,
            chain,
            spec_id,
            beneficiary_location_hash: hasher.hash_one(MemoryLocation::Basic(block_env.coinbase)),
            block_env,
            reward_policy: RewardPolicy::Ethereum, // TODO: Derive from [chain]
            // We subtract one as we don't ever retry the first transaction
            retried_tx: (0..txs.len() - 1).map(|_| AtomicU8::new(0)).collect(),
            txs: DeferDrop::new(txs),
        }
    }

    fn get_address_hash(&self, address: &Address) -> MemoryLocationHash {
        if address == &self.block_env.coinbase {
            self.beneficiary_location_hash
        } else {
            self.hasher.hash_one(MemoryLocation::Basic(*address))
        }
    }

    // Execute a transaction. This can read from memory but cannot modify any state.
    // A successful execution returns:
    //   - A write-set consisting of memory locations and their updated values.
    //   - A read-set consisting of memory locations and their origins.
    //
    // An execution may observe a read dependency on a lower transaction. This happens
    // when the last incarnation of the dependency wrote to a memory location that
    // this transaction reads, but it aborted before the read. In this case, the
    // dependency index is returned via [blocking_tx_idx]. An execution task for this
    // transaction is re-scheduled after the blocking dependency finishes its
    // next incarnation.
    //
    // When a transaction attempts to write a value to a location, the location and
    // value are added to the write set, possibly replacing a pair with a prior value
    // (if it is not the first time the transaction wrote to this location during the
    // execution).
    pub(crate) fn execute(&self, tx_idx: TxIdx) -> VmExecutionResult {
        // SATEFY: A correct scheduler would guarantee this index to be inbound.
        let tx = index!(self.txs, tx_idx);
        let from = &tx.caller;
        let from_hash = self.get_address_hash(from);
        let (is_create_tx, to, to_hash) = match &tx.transact_to {
            TransactTo::Call(address) => {
                (false, Some(address), Some(self.get_address_hash(address)))
            }
            TransactTo::Create => (true, None, None),
        };
        // TODO: The perfect condition is if the recipient is contract.
        let is_maybe_lazy = tx.data.is_empty() && Some(from) != to;

        // Execute
        let mut db = VmDb::new(self, &tx_idx, from, from_hash, to, to_hash, is_maybe_lazy);
        match execute_tx(
            &mut db,
            self.chain,
            self.spec_id,
            self.block_env.clone(),
            tx.clone(),
            false,
        ) {
            Ok(result_and_state) => {
                // We unfortunately must retry at least once on reverted transactions since it
                // may have reverted prematurely before registering the full read set that
                // would invalidate and retry incorrectly when a potential lower fulfilling
                // transactions completed.
                if matches!(result_and_state.result, ExecutionResult::Revert { .. })
                    && tx_idx > 0
                    // We subtract one as we don't ever retry the first transaction
                    // TODO: Test this aggressively to find an appropriate number of retries.
                    && index!(self.retried_tx, tx_idx - 1).fetch_add(1, Ordering::Relaxed) < 1
                {
                    return VmExecutionResult::ReadError {
                        blocking_tx_idx: tx_idx - 1,
                    };
                }

                // There are at least three locations most of the time: the sender,
                // the recipient, and the beneficiary accounts.
                // TODO: Allocate up to [result_and_state.state.len()] anyway?
                let mut write_set = WriteSet::with_capacity(3);
                for (address, account) in result_and_state.state.iter() {
                    if account.is_selfdestructed() {
                        write_set.push((
                            self.get_address_hash(address),
                            MemoryValue::Basic(Box::default()),
                        ));
                        continue;
                    }

                    if account.is_touched() {
                        let account_location_hash = self.get_address_hash(address);
                        if db.read_set.accounts.get(&account_location_hash) != Some(&account.info) {
                            // Skip transactions with the same from & to until we have lazy updates
                            // for the sender nonce & balance.
                            if is_maybe_lazy
                                && Some(address) == to
                                && account.info.is_empty_code_hash()
                            {
                                write_set.push((
                                    account_location_hash,
                                    MemoryValue::LazyBalanceAddition(tx.value),
                                ));
                            } else {
                                // TODO: More granularity here to ensure we only notify new
                                // memory writes, for instance, only an account's balance instead
                                // of the whole account.
                                write_set.push((
                                    account_location_hash,
                                    MemoryValue::Basic(Box::new(account.info.clone())),
                                ));
                            }
                        }
                    }

                    // TODO: We should move this changed check to our read set like for account info?
                    for (slot, value) in account.changed_storage_slots() {
                        write_set.push((
                            self.hasher
                                .hash_one(MemoryLocation::Storage(*address, *slot)),
                            MemoryValue::Storage(value.present_value),
                        ));
                    }
                }

                self.apply_rewards(
                    &mut write_set,
                    tx,
                    U256::from(result_and_state.result.gas_used()),
                );

                let next_validation_idx =
                    // Don't need to validate the first transaction
                    if tx_idx == 0 {
                        None
                    }
                    // Validate from this transaction if it reads something outside of its
                    // sender and to infos.
                    else if !db.only_read_from_and_to {
                        Some(tx_idx)
                    }
                    // Validate from the next transaction if doesn't read externally but
                    // deploy a new contract, or if it writes to a location outside
                    // of the beneficiary account, its sender and to infos.
                    else if is_create_tx
                        || write_set.iter().any(|(location_hash, _)| {
                            location_hash != &from_hash
                                && location_hash != &to_hash.unwrap()
                                && location_hash != &self.beneficiary_location_hash
                        })
                    {
                        Some(tx_idx + 1)
                    }
                    // Don't need to validate transactions that don't read nor write to
                    // any location outside of its read & to accounts.
                    else {
                        None
                    };

                VmExecutionResult::Ok {
                    execution_result: PevmTxExecutionResult::from_revm(
                        self.spec_id,
                        result_and_state,
                    ),
                    read_locations: db.read_set.locations,
                    write_set,
                    next_validation_idx,
                }
            }
            Err(EVMError::Database(ReadError::BlockingIndex(blocking_tx_idx))) => {
                VmExecutionResult::ReadError { blocking_tx_idx }
            }
            Err(err) => {
                // Optimistically retry in case some previous internal transactions send
                // more fund to the sender but hasn't been executed yet.
                if matches!(
                    err,
                    EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. })
                )
                    && tx_idx > 0
                    // We subtract one as we don't ever retry the first transaction
                    // TODO: Test this aggressively to find an appropriate number of retries.
                    && index!(self.retried_tx, tx_idx - 1).fetch_add(1, Ordering::Relaxed) < 1
                {
                    VmExecutionResult::ReadError {
                        blocking_tx_idx: tx_idx - 1,
                    }
                } else {
                    VmExecutionResult::ExecutionError(err)
                }
            }
        }
    }

    // Apply rewards (balance increments) to beneficiary accounts, etc.
    fn apply_rewards(&self, write_set: &mut WriteSet, tx: &TxEnv, gas_used: U256) {
        let rewards: Vec<(MemoryLocationHash, U256)> = match self.reward_policy {
            RewardPolicy::Ethereum => {
                let mut gas_price = if let Some(priority_fee) = tx.gas_priority_fee {
                    std::cmp::min(tx.gas_price, priority_fee + self.block_env.basefee)
                } else {
                    tx.gas_price
                };
                if self.spec_id.is_enabled_in(SpecId::LONDON) {
                    gas_price = gas_price.saturating_sub(self.block_env.basefee);
                }
                vec![(self.beneficiary_location_hash, gas_price * gas_used)]
            }
        };

        for (recipient, amount) in rewards {
            if let Some((_, value)) = write_set
                .iter_mut()
                .find(|(location, _)| location == &recipient)
            {
                match value {
                    MemoryValue::Basic(info) => info.balance += amount,
                    MemoryValue::LazyBalanceAddition(addition) => *addition += amount,
                    MemoryValue::Storage(_) => unreachable!(), // TODO: Better error handling
                }
            } else {
                write_set.push((recipient, MemoryValue::LazyBalanceAddition(amount)));
            }
        }
    }
}

pub(crate) fn execute_tx<DB: Database>(
    db: DB,
    chain: Chain,
    spec_id: SpecId,
    block_env: BlockEnv,
    tx: TxEnv,
    with_reward_beneficiary: bool,
) -> Result<ResultAndState, EVMError<DB::Error>> {
    // This is much uglier than the builder interface but can be up to 50% faster!!
    let context = Context {
        evm: EvmContext::new_with_env(
            db,
            Env::boxed(CfgEnv::default().with_chain_id(chain.id()), block_env, tx),
        ),
        external: (),
    };
    // TODO: Support OP handlers
    let handler = Handler::mainnet_with_spec(spec_id, with_reward_beneficiary);
    Evm::new(context, handler).transact()
}
