use std::{
    cmp::min,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
};

use crossbeam::utils::CachePadded;

use crate::{
    IncarnationStatus, Task, TransactionsDependenciesNum, TransactionsDependents,
    TransactionsStatus, TxIdx, TxStatus, TxVersion,
};

// The Pevm collaborative scheduler coordinates execution & validation
// tasks among work threads.
//
// To pick a task, threads increment the smaller of the (execution and
// validation) task counters until they find a task that is ready to be
// performed. To redo a task for a transaction, the thread updates the status
// and reduces the corresponding counter to the transaction index if it had a
// larger value.
//
// An incarnation may write to a memory location that was previously
// read by a higher transaction. Thus, when an incarnation finishes, new
// validation tasks are created for higher transactions.
//
// Validation tasks are scheduled optimistically and in parallel. Identifying
// validation failures and aborting incarnations as soon as possible is critical
// for performance, as any incarnation that reads values written by an
// incarnation that aborts also must abort.
// When an incarnation writes only to a subset of memory locations written
// by the previously completed incarnation of the same transaction, we schedule
// validation just for the incarnation. This is sufficient as the whole write
// set of the previous incarnation is marked as ESTIMATE during the abort.
// The abort leads to optimistically creating validation tasks for higher
// transactions. Threads that perform these tasks can already detect validation
// failure due to the ESTIMATE markers on memory locations, instead of waiting
// for a subsequent incarnation to finish.
//
// To fight false-sharing, instead of blindly padding everything, we run
// $ CARGO_PROFILE_BENCH_DEBUG=true cargo bench --bench mainnet
// $ perf record -e cache-misses target/release/deps/mainnet-??? --bench
// $ perf report
// To identify then pad atomics with the highest overheads.
// Current tops:
// - `execution_idx`
// - `validation_idx`
// - The ones inside `transactions_status` `Mutex`es
// We also align the struct and each field up to `transactions_status`
// to start at a new 64-or-128-bytes cache line.
#[repr(align(128))]
pub(crate) struct Scheduler {
    // The next transaction to try and execute.
    execution_idx: CachePadded<AtomicUsize>,
    // The next transaction to try and validate.
    validation_idx: CachePadded<AtomicUsize>,
    // The most up-to-date incarnation number (initially 0) and
    // the status of this incarnation.
    // TODO: Consider packing [TxStatus]s into atomics instead of
    // [Mutex] given how small they are.
    transactions_status: Vec<CachePadded<Mutex<TxStatus>>>,
    // The number of transactions in this block.
    block_size: usize,
    // We won't validate until we find the first transaction that
    // reads or writes outside of its preprocessed dependencies.
    min_validation_idx: AtomicUsize,
    // The number of validated transactions
    num_validated: AtomicUsize,
    // The list of dependent transactions to resume when the
    // key transaction is re-executed.
    transactions_dependents: Vec<Mutex<Vec<TxIdx>>>,
    // An optional number of tx dependencies flagged during preprocessing.
    // TODO: we can make [add_dependency] take in a list instead of just the first estimated one.
    transactions_dependencies_num: Vec<AtomicUsize>,
}

impl Scheduler {
    pub(crate) fn new(
        block_size: usize,
        transactions_status: TransactionsStatus,
        transactions_dependents: TransactionsDependents,
        transactions_dependencies: TransactionsDependenciesNum,
    ) -> Self {
        Self {
            block_size,
            execution_idx: CachePadded::new(AtomicUsize::new(0)),
            transactions_status: transactions_status
                .into_iter()
                .map(|status| CachePadded::new(Mutex::new(status)))
                .collect(),
            transactions_dependents: transactions_dependents
                .into_iter()
                .map(Mutex::new)
                .collect(),
            transactions_dependencies_num: transactions_dependencies
                .into_iter()
                .map(AtomicUsize::new)
                .collect(),
            // We won't validate until we find the first transaction that
            // reads or writes outside of its preprocessed dependencies.
            validation_idx: CachePadded::new(AtomicUsize::new(block_size)),
            min_validation_idx: AtomicUsize::new(block_size),
            num_validated: AtomicUsize::new(0),
        }
    }

    fn try_execute(&self, mut tx_idx: TxIdx) -> Option<TxVersion> {
        while tx_idx < self.block_size {
            let mut tx = index_mutex!(self.transactions_status, tx_idx);
            if tx.status == IncarnationStatus::ReadyToExecute {
                tx.status = IncarnationStatus::Executing;
                return Some(TxVersion {
                    tx_idx,
                    tx_incarnation: tx.incarnation,
                });
            }
            drop(tx);
            tx_idx = self.execution_idx.fetch_add(1, Ordering::Release);
        }
        None
    }

    fn try_validate(&self) -> Option<TxVersion> {
        let tx_idx = self.validation_idx.fetch_add(1, Ordering::Release);
        if tx_idx < self.block_size {
            let tx = index_mutex!(self.transactions_status, tx_idx);
            if matches!(
                tx.status,
                IncarnationStatus::Executed | IncarnationStatus::Validated
            ) {
                return Some(TxVersion {
                    tx_idx,
                    tx_incarnation: tx.incarnation,
                });
            }
        }
        None
    }

    pub(crate) fn next_task(&self) -> Option<Task> {
        loop {
            let execution_idx = self.execution_idx.load(Ordering::Acquire);
            let validation_idx = self.validation_idx.load(Ordering::Acquire);
            if execution_idx >= self.block_size && validation_idx >= self.block_size {
                if self.num_validated.load(Ordering::Acquire)
                    >= self.block_size - self.min_validation_idx.load(Ordering::Acquire)
                {
                    break;
                }
                continue;
            }

            if validation_idx < execution_idx {
                if let Some(tx_version) = self.try_validate() {
                    return Some(Task::Validation(tx_version));
                }
            } else if let Some(tx_version) =
                self.try_execute(self.execution_idx.fetch_add(1, Ordering::Release))
            {
                return Some(Task::Execution(tx_version));
            }
        }
        None
    }

    // Add [tx_idx] as a dependent of [blocking_tx_idx] so [tx_idx] is
    // re-executed when the next [blocking_tx_idx] incarnation is executed.
    // Return [false] if we encounter a race condition when [blocking_tx_idx]
    // gets re-executed before the dependency can be added.
    pub(crate) fn add_dependency(&self, tx_idx: TxIdx, blocking_tx_idx: TxIdx) -> bool {
        // This is an important lock to prevent a race condition where the blocking
        // transaction completes re-execution before this dependency can be added.
        let blocking_tx = index_mutex!(self.transactions_status, blocking_tx_idx);
        if matches!(
            blocking_tx.status,
            IncarnationStatus::Executed | IncarnationStatus::Validated
        ) {
            return false;
        }

        let mut tx = index_mutex!(self.transactions_status, tx_idx);
        if tx.status == IncarnationStatus::Executing {
            tx.status = IncarnationStatus::Aborting;
            drop(tx);

            let mut blocking_dependents =
                index_mutex!(self.transactions_dependents, blocking_tx_idx);
            blocking_dependents.push(tx_idx);
            drop(blocking_dependents);

            self.transactions_dependencies_num[tx_idx].fetch_add(1, Ordering::Release);

            return true;
        }

        unreachable!("Trying to abort & add dependency in non-executing state!")
    }

    fn set_ready_status(&self, tx_idx: TxIdx) {
        let mut tx = index_mutex!(self.transactions_status, tx_idx);
        if tx.status == IncarnationStatus::Aborting {
            tx.status = IncarnationStatus::ReadyToExecute;
            tx.incarnation += 1;
        } else {
            unreachable!("Trying to resume in non-aborting state!")
        }
    }

    pub(crate) fn finish_execution(
        &self,
        tx_version: TxVersion,
        wrote_new_location: bool,
        next_validation_idx: Option<TxIdx>,
    ) -> Option<Task> {
        let mut tx = index_mutex!(self.transactions_status, tx_version.tx_idx);
        if tx.status == IncarnationStatus::Executing {
            // TODO: Assert that `i` equals `tx_version.tx_incarnation`?
            tx.status = IncarnationStatus::Executed;
            drop(tx);

            // Resume dependent transactions
            let mut dependents = index_mutex!(self.transactions_dependents, tx_version.tx_idx);
            let mut min_dependent_idx = None;
            for tx_idx in dependents.iter() {
                // Skip this dependent as it has other pending dependencies.
                // Let the last one evoke it.
                if self.transactions_dependencies_num[*tx_idx].fetch_sub(1, Ordering::Release) > 1 {
                    continue;
                }
                self.set_ready_status(*tx_idx);
                min_dependent_idx = match min_dependent_idx {
                    None => Some(*tx_idx),
                    Some(min_index) => Some(min(*tx_idx, min_index)),
                }
            }
            dependents.clear();
            drop(dependents);

            if let Some(min_idx) = min_dependent_idx {
                self.execution_idx.fetch_min(min_idx, Ordering::Release);
            }

            // Decide where to validate from next
            let min_validation_idx = if let Some(tx_idx) = next_validation_idx {
                min(
                    self.min_validation_idx.fetch_min(tx_idx, Ordering::Release),
                    tx_idx,
                )
            } else {
                self.min_validation_idx.load(Ordering::Acquire)
            };
            // Have found a min validation index to even bother
            if min_validation_idx < self.block_size {
                // Must re-validate from min as this transaction is lower
                if tx_version.tx_idx < min_validation_idx {
                    if wrote_new_location {
                        self.validation_idx
                            .fetch_min(min_validation_idx, Ordering::Release);
                    }
                }
                // Validate from this transaction as it's in between min and the current
                // validation index.
                else if tx_version.tx_idx < self.validation_idx.load(Ordering::Acquire) {
                    if wrote_new_location {
                        self.validation_idx
                            .fetch_min(tx_version.tx_idx + 1, Ordering::Release);
                    }
                    return Some(Task::Validation(tx_version));
                }
                // Don't need to validate anything if the current validation index is
                // lower or equal -- it will catch up later.
            }
        } else {
            // TODO: Better error handling
            unreachable!("Trying to finish execution in a non-executing state")
        }
        None
    }

    // Return whether the abort was successful. A successful abort leads to
    // scheduling the transaction for re-execution and the higher transactions
    // for validation during [finish_validation]. The scheduler ensures that only
    // one failing validation per version can lead to a successful abort.
    pub(crate) fn try_validation_abort(&self, tx_version: &TxVersion) -> bool {
        let mut tx = index_mutex!(self.transactions_status, tx_version.tx_idx);
        if tx.status == IncarnationStatus::Validated {
            self.num_validated.fetch_sub(1, Ordering::Release);
        }

        let aborting = matches!(
            tx.status,
            IncarnationStatus::Executed | IncarnationStatus::Validated
        );
        if aborting {
            tx.status = IncarnationStatus::Aborting;
        }
        aborting
    }

    // When there is a successful abort, schedule the transaction for re-execution
    // and the higher transactions for validation. The re-execution task is returned
    // for the aborted transaction.
    pub(crate) fn finish_validation(&self, tx_version: &TxVersion, aborted: bool) -> Option<Task> {
        if aborted {
            self.set_ready_status(tx_version.tx_idx);
            self.validation_idx
                .fetch_min(tx_version.tx_idx + 1, Ordering::Release);
            if self.execution_idx.load(Ordering::Acquire) > tx_version.tx_idx {
                return self.try_execute(tx_version.tx_idx).map(Task::Execution);
            }
        } else {
            let mut tx = index_mutex!(self.transactions_status, tx_version.tx_idx);
            if tx.status == IncarnationStatus::Executed {
                tx.status = IncarnationStatus::Validated;
                self.num_validated.fetch_add(1, Ordering::Release);
            }
        }
        None
    }
}
