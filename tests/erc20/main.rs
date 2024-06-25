// Each cluster has one ERC20 contract and X families.
// Each family has Y people.
// Each person performs Z transfers to random people within the family.

#[path = "../common/mod.rs"]
pub mod common;

#[path = "./mod.rs"]
pub mod erc20;

use ahash::AHashMap;
use common::test_execute_revm;
use erc20::generate_cluster;
use pevm::{EvmAccount, InMemoryStorage};
use revm::primitives::{Address, TxEnv};

#[test]
fn erc20_independent() {
    const N: usize = 37123;
    let (mut state, txs) = generate_cluster(N, 1, 1);
    state.insert(Address::ZERO, EvmAccount::default()); // Beneficiary
    test_execute_revm(InMemoryStorage::new(state, []), txs);
}

#[test]
fn erc20_clusters() {
    const NUM_CLUSTERS: usize = 10;
    const NUM_FAMILIES_PER_CLUSTER: usize = 15;
    const NUM_PEOPLE_PER_FAMILY: usize = 15;
    const NUM_TRANSFERS_PER_PERSON: usize = 15;

    let mut final_state = AHashMap::from([(Address::ZERO, EvmAccount::default())]); // Beneficiary
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..NUM_CLUSTERS {
        let (state, txs) = generate_cluster(
            NUM_FAMILIES_PER_CLUSTER,
            NUM_PEOPLE_PER_FAMILY,
            NUM_TRANSFERS_PER_PERSON,
        );
        final_state.extend(state);
        final_txs.extend(txs);
    }
    common::test_execute_revm(InMemoryStorage::new(final_state, []), final_txs)
}
