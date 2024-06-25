//! Benchemark mocked blocks that exceed 1 Gigagas.

// TODO: More fancy benchmarks & plots.

#![allow(missing_docs)]

use std::{num::NonZeroUsize, thread};

use ahash::AHashMap;
use alloy_chains::Chain;
use alloy_primitives::{Address, U160, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pevm::{execute_revm, execute_revm_sequential, EvmAccount, InMemoryStorage};
use revm::primitives::{BlockEnv, SpecId, TransactTo, TxEnv};

// Better project structure
#[path = "../tests/common/mod.rs"]
pub mod common;

#[path = "../tests/erc20/mod.rs"]
pub mod erc20;

#[path = "../tests/uniswap/mod.rs"]
pub mod uniswap;

const GIGA_GAS: u64 = 1_000_000_000;

#[global_allocator]
static GLOBAL: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

pub fn bench(c: &mut Criterion, name: &str, state: common::ChainState, txs: Vec<TxEnv>) {
    let concurrency_level = thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let chain = Chain::mainnet();
    let spec_id = SpecId::LATEST;
    let block_env = BlockEnv::default();
    let storage = InMemoryStorage::new(state, []);
    let mut group = c.benchmark_group(name);
    group.bench_function("Sequential", |b| {
        b.iter(|| {
            execute_revm_sequential(
                black_box(storage.clone()),
                black_box(chain),
                black_box(spec_id),
                black_box(block_env.clone()),
                black_box(txs.clone()),
            )
        })
    });
    group.bench_function("Parallel", |b| {
        b.iter(|| {
            execute_revm(
                black_box(storage.clone()),
                black_box(chain),
                black_box(spec_id),
                black_box(block_env.clone()),
                black_box(txs.clone()),
                black_box(concurrency_level),
            )
        })
    });
    group.finish();
}

pub fn bench_raw_transfers(c: &mut Criterion) {
    let block_size = (GIGA_GAS as f64 / common::RAW_TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    bench(
        c,
        "Independent Raw Transfers",
        (0..=block_size).map(common::mock_account).collect(),
        (1..=block_size)
            .map(|i| {
                let address = Address::from(U160::from(i));
                TxEnv {
                    caller: address,
                    transact_to: TransactTo::Call(address),
                    value: U256::from(1),
                    gas_limit: common::RAW_TRANSFER_GAS_LIMIT,
                    gas_price: U256::from(1),
                    ..TxEnv::default()
                }
            })
            .collect::<Vec<_>>(),
    );
}

pub fn bench_erc20(c: &mut Criterion) {
    let block_size = (GIGA_GAS as f64 / erc20::GAS_LIMIT as f64).ceil() as usize;
    let (mut state, txs) = erc20::generate_cluster(block_size, 1, 1);
    state.insert(Address::ZERO, EvmAccount::default()); // Beneficiary
    bench(c, "Independent ERC20", state, txs);
}

pub fn bench_uniswap(c: &mut Criterion) {
    let block_size = (GIGA_GAS as f64 / uniswap::GAS_LIMIT as f64).ceil() as usize;
    let mut final_state = AHashMap::from([(Address::ZERO, EvmAccount::default())]); // Beneficiary
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..block_size {
        let (state, txs) = uniswap::generate_cluster(1, 1);
        final_state.extend(state);
        final_txs.extend(txs);
    }
    bench(c, "Independent Uniswap", final_state, final_txs);
}

pub fn benchmark_gigagas(c: &mut Criterion) {
    bench_raw_transfers(c);
    bench_erc20(c);
    bench_uniswap(c);
}

criterion_group!(benches, benchmark_gigagas);
criterion_main!(benches);
