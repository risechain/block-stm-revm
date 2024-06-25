//! Launch K clusters.
//! Each cluster has M people.
//! Each person makes N swaps.

#[path = "../common/mod.rs"]
pub mod common;

#[path = "../erc20/mod.rs"]
pub mod erc20;

#[path = "./mod.rs"]
pub mod uniswap;

use crate::uniswap::generate_cluster;
use ahash::AHashMap;
use pevm::{EvmAccount, InMemoryStorage};
use revm::primitives::{Address, TxEnv};

#[test]
fn uniswap_clusters() {
    const NUM_CLUSTERS: usize = 20;
    const NUM_PEOPLE_PER_CLUSTER: usize = 20;
    const NUM_SWAPS_PER_PERSON: usize = 20;

    let mut final_state = AHashMap::from([(Address::ZERO, EvmAccount::default())]); // Beneficiary
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..NUM_CLUSTERS {
        let (state, txs) = generate_cluster(NUM_PEOPLE_PER_CLUSTER, NUM_SWAPS_PER_PERSON);
        final_state.extend(state);
        final_txs.extend(txs);
    }
    common::test_execute_revm(InMemoryStorage::new(final_state, []), final_txs)
}
