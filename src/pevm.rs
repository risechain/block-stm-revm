use std::{
    fmt::Debug,
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock},
    thread,
};

use ahash::{AHashMap, AHashSet};
use alloy_primitives::{Address, U256};
use alloy_rpc_types::Block;
use revm::{
    db::CacheDB,
    primitives::{Account, AccountInfo, BlockEnv, ResultAndState, SpecId, TransactTo, TxEnv},
    DatabaseCommit,
};

use crate::{
    mv_memory::MvMemory,
    primitives::{get_block_env, get_block_spec, get_tx_envs},
    scheduler::Scheduler,
    storage::StorageWrapper,
    vm::{execute_tx, ExecutionError, Vm, VmExecutionResult},
    ExecutionTask, MemoryLocation, MemoryValue, Storage, Task, TransactionsDependencies,
    TransactionsDependents, TransactionsStatus, TxIdx, TxIncarnationStatus, TxVersion,
    ValidationTask,
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
    /// EVM execution error.
    // TODO: More concrete types than just an arbitrary string.
    ExecutionError(String),
    /// Impractical errors that should be unreachable.
    /// The library has bugs if this is yielded.
    UnreachableError,
}

/// Execution result of PEVM.
pub type PevmResult = Result<Vec<ResultAndState>, PevmError>;

/// Execute an Alloy block, which is becoming the "standard" format in Rust.
/// TODO: Better error handling.
pub fn execute<S: Storage + Send + Sync>(
    storage: S,
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
    let Some(tx_envs) = get_tx_envs(&block.transactions) else {
        return Err(PevmError::MissingTransactionData);
    };

    // TODO: Continue to fine tune this condition.
    // For instance, to still execute sequentially when used gas is high
    // but preprocessing yields little to no parallelism.
    if force_sequential || tx_envs.len() < 4 || block.header.gas_used <= 650_000 {
        execute_revm_sequential(storage, spec_id, block_env, tx_envs)
    } else {
        execute_revm(storage, spec_id, block_env, tx_envs, concurrency_level)
    }
}

/// Execute an REVM block.
// TODO: Do not expose this. Only exposing for ease of testing.
// The end goal is to mock Alloy blocks for test and not leak
// REVM anywhere.
pub fn execute_revm<S: Storage + Send + Sync>(
    storage: S,
    spec_id: SpecId,
    block_env: BlockEnv,
    txs: Vec<TxEnv>,
    concurrency_level: NonZeroUsize,
) -> PevmResult {
    if txs.is_empty() {
        return PevmResult::Ok(Vec::new());
    }

    let beneficiary_address = block_env.coinbase;
    let Some((scheduler, max_concurrency_level)) =
        preprocess_dependencies(&beneficiary_address, &txs)
    else {
        return execute_revm_sequential(storage, spec_id, block_env, txs);
    };

    let mut beneficiary_account_info = match storage.basic(beneficiary_address) {
        Ok(Some(account)) => account.into(),
        _ => AccountInfo::default(),
    };

    let block_size = txs.len();
    let mv_memory = Arc::new(MvMemory::new(
        block_size,
        MemoryLocation::Basic(beneficiary_address),
    ));
    let vm = Vm::new(spec_id, block_env, txs, storage, mv_memory.clone());

    let mut execution_error = OnceLock::new();
    let execution_results = (0..block_size).map(|_| Mutex::new(None)).collect();

    // TODO: Better thread handling
    thread::scope(|scope| {
        for _ in 0..concurrency_level.min(max_concurrency_level).into() {
            scope.spawn(|| {
                let mut task = None;
                let mut consecutive_empty_tasks: u8 = 0;
                while !scheduler.done() {
                    // TODO: Have different functions or an enum for the caller to choose
                    // the handling behaviour when a transaction's EVM execution fails.
                    // Parallel block builders would like to exclude such transaction,
                    // verifiers may want to exit early to save CPU cycles, while testers
                    // may want to collect all execution results. We are exiting early as
                    // the default behaviour for now.
                    if execution_error.get().is_some() {
                        break;
                    }

                    // Find and perform the next execution or validation task.
                    //
                    // After an incarnation executes it needs to pass validation. The
                    // validation re-reads the read-set and compares the observed versions.
                    // A successful validation implies that the applied writes are still
                    // up-to-date. A failed validation means the incarnation must be
                    // aborted and the transaction is re-executed in a next one.
                    //
                    // A successful validation does not guarantee that an incarnation can be
                    // committed. Since an abortion and re-execution of an earlier transaction
                    // in the block might invalidate the incarnation read set and necessitate
                    // re-execution. Thus, when a transaction aborts, all higher transactions
                    // are scheduled for re-validation. The same incarnation may be validated
                    // multiple times, by different threads, and potentially in parallel, but
                    // BlockSTM ensures that only the first abort per version succeeds.
                    //
                    // Since transactions must be committed in order, BlockSTM prioritizes
                    // tasks associated with lower-indexed transactions.
                    task = match task {
                        Some(Task::Execution(tx_version)) => try_execute(
                            &mv_memory,
                            &vm,
                            &scheduler,
                            &execution_error,
                            &execution_results,
                            tx_version,
                        )
                        .map(Task::Validation),
                        Some(Task::Validation(tx_version)) => {
                            try_validate(&mv_memory, &scheduler, &tx_version).map(Task::Execution)
                        }
                        None => scheduler.next_task(),
                    };

                    if task.is_none() {
                        consecutive_empty_tasks += 1;
                    } else {
                        consecutive_empty_tasks = 0;
                    }
                    // Many consecutive empty tasks usually mean the number of remaining tasks
                    // is smaller than the number of threads, or they are highly sequential
                    // anyway. This early exit helps remove thread overheads and join faster.
                    if consecutive_empty_tasks == 3 {
                        break;
                    }
                }
            });
        }
    });

    if let Some(err) = execution_error.take() {
        return Err(PevmError::ExecutionError(format!("{err:?}")));
    }

    // We lazily evaluate the final beneficiary account's balance at the end of each transaction
    // to avoid "implicit" dependency among consecutive transactions that read & write there.
    // TODO: Refactor, improve speed & error handling.
    let beneficiary_values = mv_memory.consume_beneficiary();
    Ok(execution_results
        .into_iter()
        .zip(beneficiary_values)
        .map(|(mutex, value)| {
            let mut result_and_state = mutex.into_inner().unwrap().unwrap();
            result_and_state.state.insert(
                beneficiary_address,
                post_process_beneficiary(&mut beneficiary_account_info, value),
            );
            result_and_state
        })
        .collect())
}

/// Execute REVM transactions sequentially.
// Useful for fallback back for a small block,
// TODO: Use this for a long chain of sequential transactions even in parallel mode.
pub fn execute_revm_sequential<S: Storage>(
    storage: S,
    spec_id: SpecId,
    block_env: BlockEnv,
    txs: Vec<TxEnv>,
) -> Result<Vec<ResultAndState>, PevmError> {
    let mut results = Vec::with_capacity(txs.len());
    let mut db = CacheDB::new(StorageWrapper(storage));
    for tx in txs {
        match execute_tx(&mut db, spec_id, block_env.clone(), tx, true) {
            Ok(result_and_state) => {
                db.commit(result_and_state.state.clone());
                results.push(result_and_state);
            }
            Err(err) => return Err(PevmError::ExecutionError(format!("{err:?}"))),
        }
    }
    Ok(results)
}

// Return `None` to signal falling back to sequential execution as we detected too many
// dependencies. Otherwise return a tuned scheduler and the max concurrency level.
// TODO: Clearer interface & make this as fast as possible.
// For instance, to use an enum return type and `SmallVec` over `AHashSet`.
fn preprocess_dependencies(
    beneficiary_address: &Address,
    txs: &[TxEnv],
) -> Option<(Scheduler, NonZeroUsize)> {
    let block_size = txs.len();

    let mut transactions_status: TransactionsStatus = (0..block_size)
        .map(|_| TxIncarnationStatus::ReadyToExecute(0))
        .collect();
    let mut transactions_dependents: TransactionsDependents =
        (0..block_size).map(|_| AHashSet::new()).collect();
    let mut transactions_dependencies: TransactionsDependencies = AHashMap::new();

    // Marking transactions across same sender & recipient as dependents as they
    // cross-depend at the `AccountInfo` level (reading & writing to nonce & balance).
    // This is critical to avoid this nasty race condition:
    // 1. A spends money
    // 2. B sends money to A
    // 3. A spends money
    // Without (3) depending on (2), (2) may race and write to A first, then (1) comes
    // second flagging (2) for re-execution and execute (3) as dependency. (3) would
    // panic with a nonce error reading from (2) before it rewrites the new nonce
    // reading from (1).
    let mut tx_idxes_by_address: AHashMap<Address, Vec<TxIdx>> = AHashMap::new();
    // We evaluate from the first transaction with data, since raw transfers' dependencies
    // are already properly ordered here.
    let mut starting_validation_idx = block_size;
    for (tx_idx, tx) in txs.iter().enumerate() {
        if starting_validation_idx == block_size && tx_idx > 0 && !tx.data.is_empty() {
            starting_validation_idx = tx_idx;
        }

        // We check for a non-empty value that guarantees to update the balance of the
        // recipient, to avoid smart contract interactions that only some storage.
        let mut recipient_with_changed_balance = None;
        if let TransactTo::Call(to) = tx.transact_to {
            if tx.value != U256::ZERO {
                recipient_with_changed_balance = Some(to);
            }
        }

        // The first transaction never has dependencies.
        if tx_idx > 0 {
            // Register a lower transaction as this one's dependency.
            let mut register_dependency = |dependency_idx: usize| {
                // SAFETY: The dependency index is guaranteed to be smaller than the block
                // size in this scope.
                unsafe {
                    *transactions_status.get_unchecked_mut(tx_idx) =
                        TxIncarnationStatus::Aborting(0);
                    transactions_dependents
                        .get_unchecked_mut(dependency_idx)
                        .insert(tx_idx);
                }
                transactions_dependencies
                    .entry(tx_idx)
                    .or_default()
                    .insert(dependency_idx);
            };

            if &tx.caller == beneficiary_address
                || recipient_with_changed_balance.is_some_and(|to| &to == beneficiary_address)
            {
                register_dependency(tx_idx - 1);
            } else {
                if let Some(prev_idx) = tx_idxes_by_address
                    .get(&tx.caller)
                    .and_then(|tx_idxs| tx_idxs.last())
                {
                    register_dependency(*prev_idx);
                }
                if let Some(to) = recipient_with_changed_balance {
                    if let Some(prev_idx) = tx_idxes_by_address
                        .get(&to)
                        .and_then(|tx_idxs| tx_idxs.last())
                    {
                        register_dependency(*prev_idx);
                    }
                }
            }

            // TODO: Continue to fine tune this ratio.
            // Intuitively we should quit way before 90%.
            if transactions_dependencies.len() as f64 / block_size as f64 > 0.9 {
                return None;
            }
        }

        // Register this transaction to the sender & recipient index maps.
        tx_idxes_by_address
            .entry(tx.caller)
            .or_default()
            .push(tx_idx);
        if let Some(to) = recipient_with_changed_balance {
            tx_idxes_by_address.entry(to).or_default().push(tx_idx);
        }
    }

    let min_concurrency_level = NonZeroUsize::new(2).unwrap();
    // This division by 2 means a thread must complete ~4 tasks to justify
    // its overheads.
    // TODO: Fine tune for edge cases given the dependency data above.
    let max_concurrency_level = NonZeroUsize::new(block_size / 2)
        .unwrap_or(min_concurrency_level)
        .max(min_concurrency_level);

    Some((
        Scheduler::new(
            block_size,
            transactions_status,
            transactions_dependents,
            transactions_dependencies,
            starting_validation_idx,
        ),
        max_concurrency_level,
    ))
}

// Execute the next incarnation of a transaction.
// If an ESTIMATE is read, abort and add dependency for re-execution.
// Otherwise:
// - If there is a write to a memory location to which the
//   previous finished incarnation has not written, create validation
//   tasks for all higher transactions.
// - Otherwise, return a validation task for the transaction.
fn try_execute<S: Storage>(
    mv_memory: &Arc<MvMemory>,
    vm: &Vm<S>,
    scheduler: &Scheduler,
    execution_error: &OnceLock<ExecutionError>,
    execution_results: &Vec<Mutex<Option<ResultAndState>>>,
    tx_version: TxVersion,
) -> Option<ValidationTask> {
    match vm.execute(tx_version.tx_idx) {
        VmExecutionResult::ReadError { blocking_tx_idx } => {
            if !scheduler.add_dependency(tx_version.tx_idx, blocking_tx_idx) {
                // Retry the execution immediately if the blocking transaction was
                // re-executed by the time we can add it as a dependency.
                return try_execute(
                    mv_memory,
                    vm,
                    scheduler,
                    execution_error,
                    execution_results,
                    tx_version,
                );
            }
            None
        }
        VmExecutionResult::ExecutionError(err) => {
            // TODO: Better error handling
            execution_error.set(err).unwrap();
            None
        }
        VmExecutionResult::Ok {
            result_and_state,
            read_set,
            write_set,
        } => {
            *index_mutex!(execution_results, tx_version.tx_idx) = Some(result_and_state);
            let wrote_new_location = mv_memory.record(&tx_version, read_set, write_set);
            scheduler.finish_execution(tx_version, wrote_new_location)
        }
    }
}

// Validate the last incarnation of the transaction.
// If validation fails:
// - Mark every memory value written by the incarnation as ESTIMATE.
// - Create validation tasks for all higher transactions that have
//   not been executed.
// - Return a re-execution task for this transaction with an incremented
//   incarnation.
fn try_validate(
    mv_memory: &Arc<MvMemory>,
    scheduler: &Scheduler,
    tx_version: &TxVersion,
) -> Option<ExecutionTask> {
    let read_set_valid = mv_memory.validate_read_set(tx_version.tx_idx);
    let aborted = !read_set_valid && scheduler.try_validation_abort(tx_version);
    if aborted {
        mv_memory.convert_writes_to_estimates(tx_version.tx_idx);
    }
    scheduler.finish_validation(tx_version, aborted)
}

// Fully evaluate a beneficiary account at the end of block execution,
// including lazy updating atomic balances.
// TODO: Cleaner interface & error handling
fn post_process_beneficiary(
    beneficiary_account_info: &mut AccountInfo,
    value: MemoryValue,
) -> Account {
    match value {
        MemoryValue::Basic(info) => {
            *beneficiary_account_info = *info;
        }
        MemoryValue::LazyBeneficiaryBalance(addition) => {
            beneficiary_account_info.balance += addition;
        }
        _ => unreachable!(),
    }
    // TODO: This potentially wipes beneficiary account's storage.
    // Does that happen and if so is it acceptable? A quick test with
    // REVM wipes it too!
    let mut beneficiary_account = Account::from(beneficiary_account_info.clone());
    beneficiary_account.mark_touch();
    beneficiary_account
}
