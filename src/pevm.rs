use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    iter::once,
    num::NonZeroUsize,
    sync::{Mutex, OnceLock},
    thread,
};

use ahash::AHashMap;
use alloy_chains::Chain;
use alloy_primitives::{Address, U256};
use alloy_rpc_types::{Block, BlockTransactions};
use defer_drop::DeferDrop;
use revm::{
    db::CacheDB,
    primitives::{BlockEnv, SpecId, TransactTo, TxEnv},
    DatabaseCommit,
};

use crate::{
    mv_memory::MvMemory,
    primitives::{get_block_env, get_block_spec, get_tx_env, TransactionParsingError},
    scheduler::Scheduler,
    storage::StorageWrapper,
    vm::{execute_tx, ExecutionError, PevmTxExecutionResult, Vm, VmExecutionResult},
    AccountBasic, BuildAddressHasher, BuildIdentityHasher, EvmAccount, IncarnationStatus,
    MemoryEntry, MemoryLocation, MemoryValue, Storage, Task, TransactionsDependenciesNum,
    TransactionsDependents, TransactionsStatus, TxIdx, TxStatus, TxVersion,
};

/// Errors when executing a block with PEVM.
#[derive(Debug, PartialEq)]
pub enum PevmError {
    /// Cannot derive the chain spec from the block header.
    UnknownBlockSpec,
    /// Block header lacks information for execution.
    MissingHeaderData,
    /// Transactions lack information for execution.
    MissingTransactionData,
    /// Invalid input transaction.
    InvalidTransaction(TransactionParsingError),
    /// EVM execution error.
    // TODO: More concrete types than just an arbitrary string.
    ExecutionError(String),
    /// Impractical errors that should be unreachable.
    /// The library has bugs if this is yielded.
    UnreachableError,
}

/// Execution result of a block
pub type PevmResult = Result<Vec<PevmTxExecutionResult>, PevmError>;

/// Execute an Alloy block, which is becoming the "standard" format in Rust.
/// TODO: Better error handling.
pub fn execute<S: Storage + Send + Sync>(
    storage: S,
    chain: Chain,
    block: Block,
    concurrency_level: NonZeroUsize,
    force_sequential: bool,
) -> PevmResult {
    let Some(spec_id) = get_block_spec(&block.header) else {
        return Err(PevmError::UnknownBlockSpec);
    };
    let Some(block_env) = get_block_env(&block.header) else {
        return Err(PevmError::MissingHeaderData);
    };
    let tx_envs = match block.transactions {
        BlockTransactions::Full(txs) => txs
            .into_iter()
            .map(get_tx_env)
            .collect::<Result<Vec<TxEnv>, TransactionParsingError>>()
            .map_err(PevmError::InvalidTransaction)?,
        _ => return Err(PevmError::MissingTransactionData),
    };
    // TODO: Continue to fine tune this condition.
    if force_sequential || tx_envs.len() < 4 || block.header.gas_used <= 650_000 {
        execute_revm_sequential(storage, chain, spec_id, block_env, tx_envs)
    } else {
        execute_revm(
            storage,
            chain,
            spec_id,
            block_env,
            tx_envs,
            concurrency_level,
        )
    }
}

/// Execute an REVM block.
// Ideally everyone would go through the [Alloy] interface. This one is currently
// useful for testing, and for users that are heavily tied to Revm like Reth.
pub fn execute_revm<S: Storage + Send + Sync>(
    storage: S,
    chain: Chain,
    spec_id: SpecId,
    block_env: BlockEnv,
    txs: Vec<TxEnv>,
    concurrency_level: NonZeroUsize,
) -> PevmResult {
    if txs.is_empty() {
        return Ok(Vec::new());
    }

    // Preprocess dependencies and fall back to sequential if there are too many
    let beneficiary_address = block_env.coinbase;
    let Some((scheduler, max_concurrency_level)) =
        preprocess_dependencies(&beneficiary_address, &txs)
    else {
        return execute_revm_sequential(storage, chain, spec_id, block_env, txs);
    };

    // Preprocess locations
    // TODO: Move to a dedicated preprocessing module with preprocessing deps
    let block_size = txs.len();
    let hasher = ahash::RandomState::new();
    let beneficiary_location_hash = hasher.hash_one(MemoryLocation::Basic(beneficiary_address));
    // TODO: Estimate more locations based on sender, to, etc.
    let mut estimated_locations = HashMap::with_hasher(BuildIdentityHasher::default());
    estimated_locations.insert(
        beneficiary_location_hash,
        (0..block_size).collect::<Vec<TxIdx>>(),
    );
    let lazy_to_addresses: HashSet<Address, BuildAddressHasher> = txs
        .iter()
        .filter_map(|tx| {
            if let TransactTo::Call(to_address) = tx.transact_to {
                // TODO: Unify this condition with [Vm::execute]
                // TODO: Better error handling
                if to_address != tx.caller && !storage.is_contract(&to_address).unwrap() {
                    return Some(to_address);
                }
            }
            None
        })
        .collect();

    // Initialize the remaining core components
    // TODO: Provide more explicit garbage collecting configs for users over random background
    // threads like this. For instance, to have a dedicated thread (pool) for cleanup.
    let mv_memory = DeferDrop::new(MvMemory::new(block_size, estimated_locations));
    let vm = Vm::new(
        &hasher, &storage, &mv_memory, chain, spec_id, block_env, txs,
    );

    let mut execution_error = OnceLock::new();
    let execution_results: Vec<_> = (0..block_size).map(|_| Mutex::new(None)).collect();

    // TODO: Better thread handling
    thread::scope(|scope| {
        for _ in 0..concurrency_level.min(max_concurrency_level).into() {
            scope.spawn(|| {
                let mut task = scheduler.next_task();
                while task.is_some() {
                    task = match task.unwrap() {
                        Task::Execution(tx_version) => try_execute(
                            &mv_memory,
                            &vm,
                            &scheduler,
                            &execution_error,
                            &execution_results,
                            tx_version,
                        ),
                        Task::Validation(tx_version) => {
                            try_validate(&mv_memory, &scheduler, &tx_version)
                        }
                    };

                    // TODO: Have different functions or an enum for the caller to choose
                    // the handling behaviour when a transaction's EVM execution fails.
                    // Parallel block builders would like to exclude such transaction,
                    // verifiers may want to exit early to save CPU cycles, while testers
                    // may want to collect all execution results. We are exiting early as
                    // the default behaviour for now. Also, be aware of a potential deadlock
                    // in the scheduler's next task loop when an error occurs.
                    if execution_error.get().is_some() {
                        break;
                    }

                    if task.is_none() {
                        task = scheduler.next_task();
                    }
                }
            });
        }
    });

    if let Some(err) = execution_error.take() {
        return Err(PevmError::ExecutionError(format!("{err:?}")));
    }

    let mut fully_evaluated_results = Vec::with_capacity(block_size);
    let mut cumulative_gas_used: u128 = 0;
    for mutex in execution_results {
        let mut execution_result = mutex.into_inner().unwrap().unwrap();
        cumulative_gas_used += execution_result.receipt.cumulative_gas_used;
        execution_result.receipt.cumulative_gas_used = cumulative_gas_used;
        fully_evaluated_results.push(execution_result);
    }

    // We fully evaluate (the balance and nonce of) the beneficiary account
    // and raw transfer recipients that may have been atomically updated.
    for address in lazy_to_addresses
        .into_iter()
        .chain(once(beneficiary_address))
    {
        let location_hash = hasher.hash_one(MemoryLocation::Basic(address));
        if let Some(write_history) = mv_memory.consume_location(&location_hash) {
            // TODO: We don't need to read from storage if the first entry is a fully evaluated account.
            let mut current_account = match storage.basic(&address) {
                Ok(Some(account)) => account,
                _ => AccountBasic::default(),
            };

            for (tx_idx, memory_entry) in write_history {
                match memory_entry {
                    MemoryEntry::Data(_, MemoryValue::Basic(info)) => {
                        // TODO: Can code be changed mid-block?
                        current_account.balance = info.balance;
                        current_account.nonce = info.nonce;
                    }
                    MemoryEntry::Data(_, MemoryValue::LazyBalanceAddition(addition)) => {
                        current_account.balance += addition;
                    }
                    // TODO: Better error handling
                    _ => unreachable!(),
                }

                // SAFETY: The multi-version data structure should not leak an index over block size.
                let tx_result = unsafe { fully_evaluated_results.get_unchecked_mut(tx_idx) };
                let account = tx_result.state.entry(address).or_default();
                if current_account.is_empty() {
                    *account = None;
                } else if let Some(account) = account {
                    // Explicit write: only overwrite the account info in case there are storage changes
                    // TODO: Can code be changed mid-block?
                    account.basic.balance = current_account.balance;
                    account.basic.nonce = current_account.nonce;
                } else {
                    // Implicit write: e.g. gas payments to the beneficiary account,
                    // which doesn't have explicit writes in [tx_result.state]
                    *account = Some(EvmAccount {
                        basic: current_account.clone(),
                        storage: AHashMap::default(),
                    });
                }
            }
        }
    }

    Ok(fully_evaluated_results)
}

/// Execute REVM transactions sequentially.
// Useful for falling back for (small) blocks with many dependencies.
// TODO: Use this for a long chain of sequential transactions even in parallel mode.
pub fn execute_revm_sequential<S: Storage>(
    storage: S,
    chain: Chain,
    spec_id: SpecId,
    block_env: BlockEnv,
    txs: Vec<TxEnv>,
) -> Result<Vec<PevmTxExecutionResult>, PevmError> {
    let mut db = CacheDB::new(StorageWrapper(storage));
    let mut results = Vec::with_capacity(txs.len());
    let mut cumulative_gas_used: u128 = 0;
    for tx in txs {
        match execute_tx(&mut db, chain, spec_id, block_env.clone(), tx, true) {
            Ok(result_and_state) => {
                db.commit(result_and_state.state.clone());

                let mut execution_result =
                    PevmTxExecutionResult::from_revm(spec_id, result_and_state);

                cumulative_gas_used += execution_result.receipt.cumulative_gas_used;
                execution_result.receipt.cumulative_gas_used = cumulative_gas_used;

                results.push(execution_result);
            }
            Err(err) => return Err(PevmError::ExecutionError(format!("{err:?}"))),
        }
    }
    Ok(results)
}

// Return `None` to signal falling back to sequential execution as we detected too many
// dependencies. Otherwise return a tuned scheduler and the max concurrency level.
// TODO: Clearer interface & make this as fast as possible.
// For instance, to use an enum return type.
fn preprocess_dependencies(
    beneficiary_address: &Address,
    txs: &[TxEnv],
) -> Option<(DeferDrop<Scheduler>, NonZeroUsize)> {
    let block_size = txs.len();

    let mut transactions_status: TransactionsStatus = (0..block_size)
        .map(|_| TxStatus {
            incarnation: 0,
            status: IncarnationStatus::ReadyToExecute,
        })
        .collect();
    let mut transactions_dependents: TransactionsDependents = vec![vec![]; block_size];
    let mut transactions_dependencies =
        TransactionsDependenciesNum::with_hasher(BuildIdentityHasher::default());

    // Marking transactions from a sender as dependent of the closest transaction that
    // shares the same sender and the closest that sends to this sender to avoid fatal
    // nonce & balance too low errors.
    let mut last_tx_idx_by_sender = HashMap::<Address, TxIdx, BuildAddressHasher>::default();
    let mut last_tx_idx_by_recipient = HashMap::<Address, TxIdx, BuildAddressHasher>::default();

    for (tx_idx, tx) in txs.iter().enumerate() {
        let mut register_dependency = |dependency_idxs: Vec<usize>| {
            // SAFETY: The dependency index is guaranteed to be smaller than the block
            // size in this scope.
            unsafe {
                transactions_status.get_unchecked_mut(tx_idx).status = IncarnationStatus::Aborting;
                transactions_dependencies.insert(tx_idx, dependency_idxs.len());
                for dependency_idx in dependency_idxs {
                    transactions_dependents
                        .get_unchecked_mut(dependency_idx)
                        .push(tx_idx);
                }
            }
        };

        if tx_idx > 0 {
            // Beneficiary account: depends on all transactions from the last beneficiary tx.
            if &tx.caller == beneficiary_address
                || tx.transact_to == TransactTo::Call(*beneficiary_address)
            {
                let start_idx = last_tx_idx_by_sender
                    .get(beneficiary_address)
                    .cloned()
                    .unwrap_or(0);
                register_dependency((start_idx..tx_idx).collect());
            }
            // Otherwise, build dependencies for the sender to avoid fatal errors
            else {
                let mut dependencies = Vec::new();
                if let Some(prev_idx) = last_tx_idx_by_sender.get(&tx.caller) {
                    dependencies.push(*prev_idx);
                }
                if let Some(prev_idx) = last_tx_idx_by_recipient.get(&tx.caller) {
                    if !dependencies.contains(prev_idx) {
                        dependencies.push(*prev_idx);
                    }
                }
                if !dependencies.is_empty() {
                    register_dependency(dependencies);
                }
            }
        }

        // TODO: Continue to fine tune this ratio.
        if transactions_dependencies.len() as f64 / block_size as f64 > 0.85 {
            return None;
        }

        last_tx_idx_by_sender.insert(tx.caller, tx_idx);
        if tx.value > U256::ZERO {
            if let TransactTo::Call(to_address) = tx.transact_to {
                last_tx_idx_by_recipient.insert(to_address, tx_idx);
            }
        }
    }

    let min_concurrency_level = NonZeroUsize::new(2).unwrap();
    let max_concurrency_level =
        // Diving the number of ready transactions by 2 means a thread must
        // complete ~4 tasks to justify its overheads.
        // TODO: Further fine tune given the dependency data above.
        NonZeroUsize::new((block_size - transactions_dependencies.len()) / 2)
            .unwrap_or(min_concurrency_level)
            .max(min_concurrency_level);

    Some((
        DeferDrop::new(Scheduler::new(
            block_size,
            transactions_status,
            transactions_dependents,
            transactions_dependencies,
        )),
        max_concurrency_level,
    ))
}

fn try_execute<S: Storage>(
    mv_memory: &MvMemory,
    vm: &Vm<S>,
    scheduler: &Scheduler,
    execution_error: &OnceLock<ExecutionError>,
    execution_results: &[Mutex<Option<PevmTxExecutionResult>>],
    tx_version: TxVersion,
) -> Option<Task> {
    loop {
        return match vm.execute(tx_version.tx_idx) {
            VmExecutionResult::Retry => continue,
            VmExecutionResult::ReadError { blocking_tx_idx } => {
                if !scheduler.add_dependency(tx_version.tx_idx, blocking_tx_idx) {
                    // Retry the execution immediately if the blocking transaction was
                    // re-executed by the time we can add it as a dependency.
                    continue;
                }
                None
            }
            VmExecutionResult::ExecutionError(err) => {
                // TODO: Better error handling
                execution_error.set(err).unwrap();
                None
            }
            VmExecutionResult::Ok {
                execution_result,
                read_locations,
                write_set,
                next_validation_idx,
            } => {
                *index_mutex!(execution_results, tx_version.tx_idx) = Some(execution_result);
                let wrote_new_location = mv_memory.record(&tx_version, read_locations, write_set);
                scheduler.finish_execution(tx_version, wrote_new_location, next_validation_idx)
            }
        };
    }
}

fn try_validate(
    mv_memory: &MvMemory,
    scheduler: &Scheduler,
    tx_version: &TxVersion,
) -> Option<Task> {
    let read_set_valid = mv_memory.validate_read_locations(tx_version.tx_idx);
    let aborted = !read_set_valid && scheduler.try_validation_abort(tx_version);
    if aborted {
        mv_memory.convert_writes_to_estimates(tx_version.tx_idx);
    }
    scheduler.finish_validation(tx_version, aborted)
}
