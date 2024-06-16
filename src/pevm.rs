use std::{
    fmt::Debug,
    num::NonZeroUsize,
    sync::{Mutex, OnceLock},
    thread,
};

use ahash::AHashMap;
use alloy_primitives::{Address, U256};
use alloy_rpc_types::Block;
use revm::{
    db::CacheDB,
    primitives::{AccountInfo, BlockEnv, SpecId, TransactTo, TxEnv},
    DatabaseCommit,
};

use crate::{
    mv_memory::MvMemory,
    primitives::{get_block_env, get_block_spec, get_tx_envs},
    scheduler::Scheduler,
    storage::StorageWrapper,
    vm::{execute_tx, ExecutionError, PevmTxExecutionResult, Vm, VmExecutionResult},
    EvmAccount, ExecutionTask, IncarnationStatus, MemoryLocation, MemoryValue, Storage, Task,
    TransactionsDependencies, TransactionsDependents, TransactionsStatus, TxIdx, TxStatus,
    TxVersion, ValidationTask,
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

/// Execution result of a block
pub type PevmResult = Result<Vec<PevmTxExecutionResult>, PevmError>;

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
        return Ok(Vec::new());
    }

    let beneficiary_address = block_env.coinbase;
    let Some((scheduler, max_concurrency_level)) =
        preprocess_dependencies(&beneficiary_address, &txs)
    else {
        return execute_revm_sequential(storage, spec_id, block_env, txs);
    };

    let mut beneficiary_account = match storage.basic(&beneficiary_address) {
        Ok(Some(account)) => account.into(),
        _ => AccountInfo::default(),
    };

    let block_size = txs.len();
    let mv_memory = MvMemory::new(block_size, MemoryLocation::Basic(beneficiary_address));
    let vm = Vm::new(spec_id, block_env, txs, storage, &mv_memory);

    let mut execution_error = OnceLock::new();
    let execution_results: Vec<_> = (0..block_size).map(|_| Mutex::new(None)).collect();

    // TODO: Better thread handling
    thread::scope(|scope| {
        for _ in 0..concurrency_level.min(max_concurrency_level).into() {
            scope.spawn(|| {
                // Find and perform the next execution or validation task.
                let mut task = scheduler.next_task();
                while task.is_some() {
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
                    task = match task.unwrap() {
                        Task::Execution(tx_version) => try_execute(
                            &mv_memory,
                            &vm,
                            &scheduler,
                            &execution_error,
                            &execution_results,
                            tx_version,
                        )
                        .map(Task::Validation),
                        Task::Validation(tx_version) => {
                            try_validate(&mv_memory, &scheduler, &tx_version).map(Task::Execution)
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

    // We lazily evaluate the final beneficiary account's balance at the end of each transaction
    // to avoid "implicit" dependency among consecutive transactions that read & write there.
    // TODO: Refactor, improve speed & error handling.
    let beneficiary_values = mv_memory.consume_beneficiary();
    let mut fully_evaluated_results = Vec::with_capacity(block_size);
    let mut cumulative_gas_used: u128 = 0;
    for (mutex, beneficiary_value) in execution_results.into_iter().zip(beneficiary_values) {
        let mut execution_result = mutex.into_inner().unwrap().unwrap();

        // Cumulative gas
        cumulative_gas_used += execution_result.receipt.cumulative_gas_used;
        execution_result.receipt.cumulative_gas_used = cumulative_gas_used;

        // Beneficiary account
        match beneficiary_value {
            MemoryValue::Basic(info) => {
                beneficiary_account = *info;
            }
            MemoryValue::LazyBeneficiaryBalance(addition) => {
                beneficiary_account.balance += addition;
            }
            _ => unreachable!(),
        }
        execution_result.state.insert(
            beneficiary_address,
            // Ad-hoc condition to pass Ethereum state tests. Realistically the beneficiary
            // account should not be empty.
            if beneficiary_account.is_empty() {
                None
            } else {
                Some(EvmAccount {
                    basic: beneficiary_account.clone().into(),
                    // EOA beneficiary accounts currently cannot have storage.
                    storage: Default::default(),
                })
            },
        );

        fully_evaluated_results.push(execution_result);
    }

    Ok(fully_evaluated_results)
}

/// Execute REVM transactions sequentially.
// Useful for fallback back for a small block,
// TODO: Use this for a long chain of sequential transactions even in parallel mode.
pub fn execute_revm_sequential<S: Storage>(
    storage: S,
    spec_id: SpecId,
    block_env: BlockEnv,
    txs: Vec<TxEnv>,
) -> Result<Vec<PevmTxExecutionResult>, PevmError> {
    let mut db = CacheDB::new(StorageWrapper(storage));
    let mut results = Vec::with_capacity(txs.len());
    let mut cumulative_gas_used: u128 = 0;
    for tx in txs {
        match execute_tx(&mut db, spec_id, block_env.clone(), tx, true) {
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

// TODO: Move to a dedicated preprocessing module.
const ERC20_METHOD_ID: [u8; 4] = [169, 5, 156, 187];

// Return `None` to signal falling back to sequential execution as we detected too many
// dependencies. Otherwise return a tuned scheduler and the max concurrency level.
// TODO: Move to a dedicated module with a clearer interface & make this as fast as possible.
fn preprocess_dependencies(
    beneficiary_address: &Address,
    txs: &[TxEnv],
) -> Option<(Scheduler, NonZeroUsize)> {
    let block_size = txs.len();

    let mut transactions_status: TransactionsStatus = (0..block_size)
        .map(|_| TxStatus {
            incarnation: 0,
            status: IncarnationStatus::ReadyToExecute,
        })
        .collect();
    let mut transactions_dependents: TransactionsDependents = vec![vec![]; block_size];
    let mut transactions_dependencies = TransactionsDependencies::default();

    // Marking transactions across same sender & recipient as dependents as they
    // cross-depend at the `AccountInfo` level (reading & writing to nonce & balance).
    // This is critical to avoid runtime dependencies that lead to many slow retries,
    // plus this nasty race condition:
    // 1. A spends money
    // 2. B sends money to A
    // 3. A spends money
    // Without (3) depending on (2), (2) may race and write to A first, then (1) comes
    // second flagging (2) for re-execution and execute (3) as dependency. (3) would
    // panic with a nonce error reading from (2) before it rewrites the new nonce
    // reading from (1).
    let mut last_tx_idx_by_address = AHashMap::<Address, TxIdx>::default();

    // Marking transactions interacting with an ERC-20 token (first address in the tuple)
    // balance (second address)
    let mut last_tx_idx_by_erc20 = AHashMap::<(Address, Address), TxIdx>::default();

    for (tx_idx, tx) in txs.iter().enumerate() {
        let mut recipient_with_changed_balance = None;
        let mut erc20_transfer = None;
        if let TransactTo::Call(to) = tx.transact_to {
            // We check for a non-empty value that guarantees to update the balance of the
            // recipient, to avoid smart contract interactions that only some storage.
            if tx.value != U256::ZERO {
                recipient_with_changed_balance = Some(to);
            }
            // TODO: Can we accept the collision risk on the method id, or do we need to add
            // more checks, whitelist ERC20 addresses, etc.?
            if tx.data.len() == 68 && tx.data.starts_with(&ERC20_METHOD_ID) {
                erc20_transfer = Some((to, Address::from_slice(&tx.data.slice(16..36))));
            }
        }

        // Register a lower transaction as this one's dependency.
        let mut register_dependency = |dependency_idxs: Vec<usize>| {
            if dependency_idxs.is_empty() {
                return;
            }
            // SAFETY: The dependency index is guaranteed to be smaller than the block
            // size in this scope.
            unsafe {
                transactions_status.get_unchecked_mut(tx_idx).status = IncarnationStatus::Aborting;
                for dependency_idx in dependency_idxs.iter() {
                    transactions_dependents
                        .get_unchecked_mut(*dependency_idx)
                        .push(tx_idx);
                }
                transactions_dependencies.insert(tx_idx, dependency_idxs);
            }
        };

        // Beneficiary account: depends on all transactions from the last beneficiary tx.
        if &tx.caller == beneficiary_address
            || recipient_with_changed_balance.is_some_and(|to| &to == beneficiary_address)
        {
            let start_idx = last_tx_idx_by_address
                .get(beneficiary_address)
                .cloned()
                .unwrap_or(0);
            register_dependency((start_idx..tx_idx).collect());
        } else {
            let mut dependency_idxs = Vec::new();

            // Dependencies across same senders & recipients
            if let Some(prev_idx) = last_tx_idx_by_address.get(&tx.caller) {
                dependency_idxs.push(*prev_idx);
            }
            if let Some(to) = &recipient_with_changed_balance {
                if let Some(prev_idx) = last_tx_idx_by_address.get(to) {
                    if !dependency_idxs.contains(prev_idx) {
                        dependency_idxs.push(*prev_idx);
                    }
                }
            }

            // Dependencies across ERC-20 transfers
            if let Some(erc20_transfer) = &erc20_transfer {
                if let Some(prev_idx) = last_tx_idx_by_erc20.get(&(erc20_transfer.0, tx.caller)) {
                    if !dependency_idxs.contains(prev_idx) {
                        dependency_idxs.push(*prev_idx);
                    }
                }
                if let Some(prev_idx) = last_tx_idx_by_erc20.get(erc20_transfer) {
                    if !dependency_idxs.contains(prev_idx) {
                        dependency_idxs.push(*prev_idx);
                    }
                }
            }

            register_dependency(dependency_idxs);
        }

        // TODO: Continue to fine tune this ratio.
        // Intuitively we should quit way before 90%.
        if transactions_dependencies.len() as f64 / block_size as f64 > 0.9 {
            return None;
        }

        // Register this transaction to the sender & recipient index maps.
        last_tx_idx_by_address.insert(tx.caller, tx_idx);
        if let Some(to) = recipient_with_changed_balance {
            last_tx_idx_by_address.insert(to, tx_idx);
        }
        if let Some((token, recipient)) = erc20_transfer {
            last_tx_idx_by_erc20.insert((token, tx.caller), tx_idx);
            last_tx_idx_by_erc20.insert((token, recipient), tx_idx);
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
        Scheduler::new(
            block_size,
            transactions_status,
            transactions_dependents,
            transactions_dependencies,
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
    mv_memory: &MvMemory,
    vm: &Vm<S>,
    scheduler: &Scheduler,
    execution_error: &OnceLock<ExecutionError>,
    execution_results: &[Mutex<Option<PevmTxExecutionResult>>],
    tx_version: TxVersion,
) -> Option<ValidationTask> {
    loop {
        return match vm.execute(tx_version.tx_idx) {
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
                read_set,
                write_set,
                next_validation_idx,
            } => {
                *index_mutex!(execution_results, tx_version.tx_idx) = Some(execution_result);
                let wrote_new_location = mv_memory.record(&tx_version, read_set, write_set);
                scheduler.finish_execution(tx_version, wrote_new_location, next_validation_idx)
            }
        };
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
    mv_memory: &MvMemory,
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
