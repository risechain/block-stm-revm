// Basing this off REVM's bins/revme/src/cmd/statetest/runner.rs

use block_stm_revm::{BlockSTM, Storage};
use revm::db::PlainAccount;
use revm::primitives::{
    calc_excess_blob_gas, Account, AccountInfo, Address, BlobExcessGasAndPrice, BlockEnv, Bytecode,
    ResultAndState, TransactTo, TxEnv, U256,
};
use revme::cmd::statetest::{
    merkle_trie::{log_rlp_hash, state_merkle_trie_root},
    models as smodels,
    utils::recover_address,
};
use std::{collections::HashMap, fs, num::NonZeroUsize, path::Path};

fn build_block_env(env: &smodels::Env) -> BlockEnv {
    BlockEnv {
        number: env.current_number,
        coinbase: env.current_coinbase,
        timestamp: env.current_timestamp,
        gas_limit: env.current_gas_limit,
        basefee: env.current_base_fee.unwrap_or_default(),
        difficulty: env.current_difficulty,
        prevrandao: env.current_random,
        blob_excess_gas_and_price: if let Some(current_excess_blob_gas) =
            env.current_excess_blob_gas
        {
            Some(BlobExcessGasAndPrice::new(current_excess_blob_gas.to()))
        } else if let (Some(parent_blob_gas_used), Some(parent_excess_blob_gas)) =
            (env.parent_blob_gas_used, env.parent_excess_blob_gas)
        {
            Some(BlobExcessGasAndPrice::new(calc_excess_blob_gas(
                parent_blob_gas_used.to(),
                parent_excess_blob_gas.to(),
            )))
        } else {
            None
        },
    }
}

fn build_tx_env(tx: &smodels::TransactionParts, indexes: &smodels::TxPartIndices) -> TxEnv {
    TxEnv {
        caller: if let Some(address) = tx.sender {
            address
        } else if let Some(address) = recover_address(tx.secret_key.as_slice()) {
            address
        } else {
            panic!("Failed to parse caller") // TODO: Report test name
        },
        gas_limit: tx.gas_limit[indexes.gas].saturating_to(),
        gas_price: tx.gas_price.or(tx.max_fee_per_gas).unwrap_or_default(),
        transact_to: match tx.to {
            Some(address) => TransactTo::Call(address),
            None => TransactTo::Create,
        },
        value: tx.value[indexes.value],
        data: tx.data[indexes.data].clone(),
        nonce: Some(tx.nonce.saturating_to()),
        chain_id: Some(1), // Ethereum mainnet
        access_list: tx
            .access_lists
            .get(indexes.data)
            .and_then(Option::as_deref)
            .unwrap_or_default()
            .iter()
            .map(|item| {
                (
                    item.address,
                    item.storage_keys
                        .iter()
                        .map(|key| U256::from_be_bytes(key.0))
                        .collect::<Vec<_>>(),
                )
            })
            .collect(),
        gas_priority_fee: tx.max_priority_fee_per_gas,
        blob_hashes: tx.blob_versioned_hashes.clone(),
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas,
        eof_initcodes: Vec::new(),
        eof_initcodes_hashed: HashMap::new(),
    }
}

fn run_test_unit(unit: smodels::TestUnit) {
    for (spec_name, tests) in unit.post {
        // Should REVM know and handle these better, or it is
        // truly fine to just skip them?
        if matches!(spec_name, smodels::SpecName::Unknown) {
            continue;
        }
        let spec_id = spec_name.to_spec_id();

        for test in tests {
            let mut chain_state: HashMap<Address, PlainAccount> = HashMap::new();
            let mut block_stm_storage = Storage::default();

            // Shouldn't we parse accounts as `Account` instead of `AccountInfo`
            // to have initial storage states?
            for (address, raw_info) in unit.pre.iter() {
                let code = Bytecode::new_raw(raw_info.code.clone());
                let info =
                    AccountInfo::new(raw_info.balance, raw_info.nonce, code.hash_slow(), code);
                chain_state.insert(*address, info.clone().into());
                block_stm_storage.insert_account(*address, Account::from(info));
            }

            let exec_results = BlockSTM::run(
                block_stm_storage,
                spec_id,
                build_block_env(&unit.env),
                vec![build_tx_env(&unit.transaction, &test.indexes)],
                NonZeroUsize::MIN,
            );

            // TODO: We really should test with blocks with more than 1 tx
            assert!(exec_results.len() == 1);
            let ResultAndState { result, state } = exec_results[0].clone();

            let logs_root = log_rlp_hash(result.logs());
            assert_eq!(logs_root, test.logs);

            for (address, account) in state {
                chain_state.insert(
                    address,
                    PlainAccount {
                        info: account.info,
                        storage: account
                            .storage
                            .iter()
                            .map(|(k, v)| (*k, v.present_value))
                            .collect(),
                    },
                );
            }
            let state_root = state_merkle_trie_root(chain_state.iter().map(|(k, v)| (*k, v)));
            assert_eq!(state_root, test.hash);
        }
    }
}

#[test]
fn ethereum_tests() {
    // TODO: Run the whole suite.
    // Skip tests like REVM does when it makes sense.
    // Let's document clearly why for each test that we skip.
    let path_prefix = String::from("tests/ethereum/tests/GeneralStateTests/");
    let state_tests = [
        "Pyspecs/cancun/eip1153_tstore/gas_usage.json",
        "Pyspecs/cancun/eip1153_tstore/reentrant_call.json",
        "Pyspecs/cancun/eip1153_tstore/run_until_out_of_gas.json",
        "Pyspecs/cancun/eip1153_tstore/subcall.json",
        "Pyspecs/cancun/eip5656_mcopy/mcopy_on_empty_memory.json",
        "Pyspecs/cancun/eip5656_mcopy/valid_mcopy_operations.json",
        "Pyspecs/cancun/eip7516_blobgasfee/blobbasefee_out_of_gas.json",
        "Pyspecs/cancun/eip7516_blobgasfee/blobbasefee_stack_overflow.json",
        "Pyspecs/istanbul/eip1344_chainid/chainid.json",
        "Pyspecs/shanghai/eip3855_push0/push0_before_jumpdest.json",
        "Pyspecs/shanghai/eip3855_push0/push0_during_staticcall.json",
        "Pyspecs/shanghai/eip3855_push0/push0_fill_stack.json",
        "Pyspecs/shanghai/eip3855_push0/push0_gas_cost.json",
        "Pyspecs/shanghai/eip3855_push0/push0_key_sstore.json",
        "Pyspecs/shanghai/eip3855_push0/push0_stack_overflow.json",
        "Shanghai/stEIP3651-warmcoinbase/coinbaseWarmAccountCallGas.json",
        "Shanghai/stEIP3651-warmcoinbase/coinbaseWarmAccountCallGasFail.json",
        "stCallCodes/call_OOG_additionalGasCosts1.json",
        "stEIP1559/senderBalance.json",
        "stEIP3607/initCollidingWithNonEmptyAccount.json",
        "stExample/add11_yml.json",
        "stExample/add11.json",
        "stExample/indexesOmitExample.json",
        "stMemoryStressTest/FillStack.json",
        "stPreCompiledContracts2/modexp_0_0_0_20500.json",
        "stPreCompiledContracts2/modexp_0_0_0_22000.json",
        "stPreCompiledContracts2/modexp_0_0_0_25000.json",
        "stPreCompiledContracts2/modexp_0_0_0_35000.json",
        "stRandom/randomStatetest0.json",
        "stRandom/randomStatetest1.json",
        "stRandom/randomStatetest10.json",
        "stRandom/randomStatetest102.json",
        "stRandom/randomStatetest103.json",
        "stRandom/randomStatetest104.json",
        "stRandom/randomStatetest105.json",
        "stRandom/randomStatetest106.json",
        "stRandom/randomStatetest107.json",
        "stRandom/randomStatetest108.json",
        "stRandom/randomStatetest11.json",
        "stRandom/randomStatetest110.json",
        "stRandom/randomStatetest111.json",
        "stRandom/randomStatetest112.json",
        "stRandom/randomStatetest114.json",
        "stRandom/randomStatetest115.json",
        "stRandom/randomStatetest116.json",
        "stRandom/randomStatetest117.json",
        "stRandom/randomStatetest118.json",
        "stRandom/randomStatetest119.json",
        "stRandom/randomStatetest12.json",
        "stRandom/randomStatetest120.json",
        "stRandom/randomStatetest121.json",
        "stRandom/randomStatetest122.json",
        "stRandom/randomStatetest124.json",
        "stRandom/randomStatetest125.json",
        "stRandom/randomStatetest126.json",
        "stRandom/randomStatetest129.json",
        "stRandom/randomStatetest13.json",
        "stRandom/randomStatetest130.json",
        "stRandom/randomStatetest131.json",
        "stRandom/randomStatetest133.json",
        "stRandom/randomStatetest134.json",
        "stRandom/randomStatetest135.json",
        "stRandom/randomStatetest137.json",
        "stRandom/randomStatetest139.json",
        "stRandom/randomStatetest142.json",
        "stRandom/randomStatetest143.json",
        "stRandom/randomStatetest144.json",
        "stRandom/randomStatetest145.json",
        "stRandom/randomStatetest148.json",
        "stRandom/randomStatetest149.json",
        "stRandom/randomStatetest15.json",
        "stRandom/randomStatetest150.json",
        "stRandom/randomStatetest151.json",
        "stRandom/randomStatetest153.json",
        "stRandom/randomStatetest154.json",
        "stRandom/randomStatetest155.json",
        "stRandom/randomStatetest156.json",
        "stRandom/randomStatetest157.json",
        "stRandom/randomStatetest158.json",
        "stRandom/randomStatetest159.json",
        "stRandom/randomStatetest161.json",
        "stRandom/randomStatetest162.json",
        "stRandom/randomStatetest163.json",
        "stRandom/randomStatetest164.json",
        "stRandom/randomStatetest166.json",
        "stRandom/randomStatetest167.json",
        "stRandom/randomStatetest169.json",
        "stRandom/randomStatetest171.json",
        "stRandom/randomStatetest172.json",
        "stRandom/randomStatetest174.json",
        "stRandom/randomStatetest175.json",
        "stRandom/randomStatetest176.json",
        "stRandom/randomStatetest178.json",
        "stRandom/randomStatetest179.json",
        "stRandom/randomStatetest18.json",
        "stRandom/randomStatetest180.json",
        "stRandom/randomStatetest183.json",
        "stRandom/randomStatetest184.json",
        "stRandom/randomStatetest185.json",
        "stRandom/randomStatetest187.json",
        "stRandom/randomStatetest188.json",
        "stRandom/randomStatetest189.json",
        "stRandom/randomStatetest19.json",
        "stRandom/randomStatetest190.json",
        "stRandom/randomStatetest191.json",
        "stRandom/randomStatetest192.json",
        "stRandom/randomStatetest194.json",
        "stRandom/randomStatetest195.json",
        "stRandom/randomStatetest196.json",
        "stRandom/randomStatetest197.json",
        "stRandom/randomStatetest199.json",
        "stRandom/randomStatetest2.json",
        "stRandom/randomStatetest20.json",
        "stRandom/randomStatetest200.json",
        "stRandom/randomStatetest202.json",
        "stRandom/randomStatetest204.json",
        "stRandom/randomStatetest205.json",
        "stRandom/randomStatetest206.json",
        "stRandom/randomStatetest207.json",
        "stRandom/randomStatetest208.json",
        "stRandom/randomStatetest209.json",
        "stRandom/randomStatetest210.json",
        "stRandom/randomStatetest211.json",
        "stRandom/randomStatetest214.json",
        "stRandom/randomStatetest215.json",
        "stRandom/randomStatetest216.json",
        "stRandom/randomStatetest217.json",
        "stRandom/randomStatetest219.json",
        "stRandom/randomStatetest220.json",
        "stRandom/randomStatetest221.json",
        "stRandom/randomStatetest222.json",
        "stRandom/randomStatetest225.json",
        "stRandom/randomStatetest226.json",
        "stRandom/randomStatetest227.json",
        "stRandom/randomStatetest23.json",
        "stRandom/randomStatetest230.json",
        "stRandom/randomStatetest231.json",
        "stRandom/randomStatetest233.json",
        "stRandom/randomStatetest236.json",
        "stRandom/randomStatetest237.json",
        "stRandom/randomStatetest238.json",
        "stRandom/randomStatetest24.json",
        "stRandom/randomStatetest242.json",
        "stRandom/randomStatetest243.json",
        "stRandom/randomStatetest244.json",
        "stRandom/randomStatetest245.json",
        "stRandom/randomStatetest246.json",
        "stRandom/randomStatetest247.json",
        "stRandom/randomStatetest249.json",
        "stRandom/randomStatetest25.json",
        "stRandom/randomStatetest250.json",
        "stRandom/randomStatetest251.json",
        "stRandom/randomStatetest252.json",
        "stRandom/randomStatetest254.json",
        "stRandom/randomStatetest257.json",
        "stRandom/randomStatetest259.json",
        "stRandom/randomStatetest26.json",
        "stRandom/randomStatetest260.json",
        "stRandom/randomStatetest263.json",
        "stRandom/randomStatetest264.json",
        "stRandom/randomStatetest266.json",
        "stRandom/randomStatetest267.json",
        "stRandom/randomStatetest268.json",
        "stRandom/randomStatetest269.json",
        "stRandom/randomStatetest27.json",
        "stRandom/randomStatetest270.json",
        "stRandom/randomStatetest271.json",
        "stRandom/randomStatetest274.json",
        "stRandom/randomStatetest275.json",
        "stRandom/randomStatetest276.json",
        "stRandom/randomStatetest278.json",
        "stRandom/randomStatetest279.json",
        "stRandom/randomStatetest28.json",
        "stRandom/randomStatetest280.json",
        "stRandom/randomStatetest281.json",
        "stRandom/randomStatetest283.json",
        "stRandom/randomStatetest285.json",
        "stRandom/randomStatetest286.json",
        "stRandom/randomStatetest288.json",
        "stRandom/randomStatetest29.json",
        "stRandom/randomStatetest290.json",
        "stRandom/randomStatetest291.json",
        "stRandom/randomStatetest292.json",
        "stRandom/randomStatetest293.json",
        "stRandom/randomStatetest294.json",
        "stRandom/randomStatetest296.json",
        "stRandom/randomStatetest297.json",
        "stRandom/randomStatetest298.json",
        "stRandom/randomStatetest299.json",
        "stRandom/randomStatetest3.json",
        "stRandom/randomStatetest30.json",
        "stRandom/randomStatetest300.json",
        "stRandom/randomStatetest301.json",
        "stRandom/randomStatetest302.json",
        "stRandom/randomStatetest303.json",
        "stRandom/randomStatetest304.json",
        "stRandom/randomStatetest305.json",
        "stRandom/randomStatetest306.json",
        "stRandom/randomStatetest308.json",
        "stRandom/randomStatetest309.json",
        "stRandom/randomStatetest31.json",
        "stRandom/randomStatetest310.json",
        "stRandom/randomStatetest311.json",
        "stRandom/randomStatetest312.json",
        "stRandom/randomStatetest313.json",
        "stRandom/randomStatetest315.json",
        "stRandom/randomStatetest316.json",
        "stRandom/randomStatetest318.json",
        "stRandom/randomStatetest321.json",
        "stRandom/randomStatetest322.json",
        "stRandom/randomStatetest323.json",
        "stRandom/randomStatetest325.json",
        "stRandom/randomStatetest326.json",
        "stRandom/randomStatetest327.json",
        "stRandom/randomStatetest329.json",
        "stRandom/randomStatetest33.json",
        "stRandom/randomStatetest332.json",
        "stRandom/randomStatetest333.json",
        "stRandom/randomStatetest334.json",
        "stRandom/randomStatetest335.json",
        "stRandom/randomStatetest336.json",
        "stRandom/randomStatetest337.json",
        "stRandom/randomStatetest338.json",
        "stRandom/randomStatetest339.json",
        "stRandom/randomStatetest340.json",
        "stRandom/randomStatetest341.json",
        "stRandom/randomStatetest342.json",
        "stRandom/randomStatetest343.json",
        "stRandom/randomStatetest345.json",
        "stRandom/randomStatetest346.json",
        "stRandom/randomStatetest347.json",
        "stRandom/randomStatetest348.json",
        "stRandom/randomStatetest349.json",
        "stRandom/randomStatetest350.json",
        "stRandom/randomStatetest351.json",
        "stRandom/randomStatetest352.json",
        "stRandom/randomStatetest353.json",
        "stRandom/randomStatetest354.json",
        "stRandom/randomStatetest355.json",
        "stRandom/randomStatetest356.json",
        "stRandom/randomStatetest357.json",
        "stRandom/randomStatetest358.json",
        "stRandom/randomStatetest359.json",
        "stRandom/randomStatetest360.json",
        "stRandom/randomStatetest361.json",
        "stRandom/randomStatetest362.json",
        "stRandom/randomStatetest363.json",
        "stRandom/randomStatetest364.json",
        "stRandom/randomStatetest366.json",
        "stRandom/randomStatetest367.json",
        "stRandom/randomStatetest369.json",
        "stRandom/randomStatetest37.json",
        "stRandom/randomStatetest370.json",
        "stRandom/randomStatetest371.json",
        "stRandom/randomStatetest372.json",
        "stRandom/randomStatetest378.json",
        "stRandom/randomStatetest379.json",
        "stRandom/randomStatetest380.json",
        "stRandom/randomStatetest381.json",
        "stRandom/randomStatetest382.json",
        "stRandom/randomStatetest383.json",
        "stRandom/randomStatetest39.json",
        "stRandom/randomStatetest4.json",
        "stRandom/randomStatetest41.json",
        "stRandom/randomStatetest42.json",
        "stRandom/randomStatetest45.json",
        "stRandom/randomStatetest47.json",
        "stRandom/randomStatetest48.json",
        "stRandom/randomStatetest49.json",
        "stRandom/randomStatetest5.json",
        "stRandom/randomStatetest51.json",
        "stRandom/randomStatetest52.json",
        "stRandom/randomStatetest53.json",
        "stRandom/randomStatetest54.json",
        "stRandom/randomStatetest55.json",
        "stRandom/randomStatetest57.json",
        "stRandom/randomStatetest58.json",
        "stRandom/randomStatetest59.json",
        "stRandom/randomStatetest6.json",
        "stRandom/randomStatetest60.json",
        "stRandom/randomStatetest62.json",
        "stRandom/randomStatetest63.json",
        "stRandom/randomStatetest64.json",
        "stRandom/randomStatetest66.json",
        "stRandom/randomStatetest67.json",
        "stRandom/randomStatetest69.json",
        "stRandom/randomStatetest72.json",
        "stRandom/randomStatetest73.json",
        "stRandom/randomStatetest74.json",
        "stRandom/randomStatetest75.json",
        "stRandom/randomStatetest77.json",
        "stRandom/randomStatetest78.json",
        "stRandom/randomStatetest80.json",
        "stRandom/randomStatetest81.json",
        "stRandom/randomStatetest82.json",
        "stRandom/randomStatetest83.json",
        "stRandom/randomStatetest84.json",
        "stRandom/randomStatetest87.json",
        "stRandom/randomStatetest88.json",
        "stRandom/randomStatetest89.json",
        "stRandom/randomStatetest9.json",
        "stRandom/randomStatetest90.json",
        "stRandom/randomStatetest92.json",
        "stRandom/randomStatetest95.json",
        "stRandom/randomStatetest96.json",
        "stRandom/randomStatetest97.json",
        "stRandom/randomStatetest98.json",
        "stRandom2/randomStatetest.json",
        "stRandom2/randomStatetest384.json",
        "stRandom2/randomStatetest385.json",
        "stRandom2/randomStatetest386.json",
        "stRandom2/randomStatetest387.json",
        "stRandom2/randomStatetest388.json",
        "stRandom2/randomStatetest389.json",
        "stRandom2/randomStatetest393.json",
        "stRandom2/randomStatetest395.json",
        "stRandom2/randomStatetest396.json",
        "stRandom2/randomStatetest397.json",
        "stRandom2/randomStatetest398.json",
        "stRandom2/randomStatetest399.json",
        "stRandom2/randomStatetest402.json",
        "stRandom2/randomStatetest404.json",
        "stRandom2/randomStatetest405.json",
        "stRandom2/randomStatetest406.json",
        "stRandom2/randomStatetest407.json",
        "stRandom2/randomStatetest408.json",
        "stRandom2/randomStatetest410.json",
        "stRandom2/randomStatetest411.json",
        "stRandom2/randomStatetest412.json",
        "stRandom2/randomStatetest413.json",
        "stRandom2/randomStatetest414.json",
        "stRandom2/randomStatetest415.json",
        "stRandom2/randomStatetest416.json",
        "stRandom2/randomStatetest419.json",
        "stRandom2/randomStatetest420.json",
        "stRandom2/randomStatetest421.json",
        "stRandom2/randomStatetest422.json",
        "stRandom2/randomStatetest424.json",
        "stRandom2/randomStatetest425.json",
        "stRandom2/randomStatetest426.json",
        "stRandom2/randomStatetest428.json",
        "stRandom2/randomStatetest429.json",
        "stRandom2/randomStatetest430.json",
        "stRandom2/randomStatetest433.json",
        "stRandom2/randomStatetest435.json",
        "stRandom2/randomStatetest436.json",
        "stRandom2/randomStatetest437.json",
        "stRandom2/randomStatetest438.json",
        "stRandom2/randomStatetest439.json",
        "stRandom2/randomStatetest440.json",
        "stRandom2/randomStatetest442.json",
        "stRandom2/randomStatetest443.json",
        "stRandom2/randomStatetest444.json",
        "stRandom2/randomStatetest445.json",
        "stRandom2/randomStatetest446.json",
        "stRandom2/randomStatetest447.json",
        "stRandom2/randomStatetest448.json",
        "stRandom2/randomStatetest449.json",
        "stRandom2/randomStatetest450.json",
        "stRandom2/randomStatetest451.json",
        "stRandom2/randomStatetest452.json",
        "stRandom2/randomStatetest454.json",
        "stRandom2/randomStatetest455.json",
        "stRandom2/randomStatetest456.json",
        "stRandom2/randomStatetest457.json",
        "stRandom2/randomStatetest458.json",
        "stRandom2/randomStatetest460.json",
        "stRandom2/randomStatetest461.json",
        "stRandom2/randomStatetest462.json",
        "stRandom2/randomStatetest464.json",
        "stRandom2/randomStatetest465.json",
        "stRandom2/randomStatetest466.json",
        "stRandom2/randomStatetest467.json",
        "stRandom2/randomStatetest469.json",
        "stRandom2/randomStatetest470.json",
        "stRandom2/randomStatetest471.json",
        "stRandom2/randomStatetest472.json",
        "stRandom2/randomStatetest474.json",
        "stRandom2/randomStatetest475.json",
        "stRandom2/randomStatetest477.json",
        "stRandom2/randomStatetest478.json",
        "stRandom2/randomStatetest480.json",
        "stRandom2/randomStatetest481.json",
        "stRandom2/randomStatetest482.json",
        "stRandom2/randomStatetest483.json",
        "stRandom2/randomStatetest484.json",
        "stRandom2/randomStatetest485.json",
        "stRandom2/randomStatetest488.json",
        "stRandom2/randomStatetest489.json",
        "stRandom2/randomStatetest491.json",
        "stRandom2/randomStatetest493.json",
        "stRandom2/randomStatetest494.json",
        "stRandom2/randomStatetest496.json",
        "stRandom2/randomStatetest497.json",
        "stRandom2/randomStatetest498.json",
        "stRandom2/randomStatetest499.json",
        "stRandom2/randomStatetest500.json",
        "stRandom2/randomStatetest501.json",
        "stRandom2/randomStatetest502.json",
        "stRandom2/randomStatetest503.json",
        "stRandom2/randomStatetest504.json",
        "stRandom2/randomStatetest505.json",
        "stRandom2/randomStatetest506.json",
        "stRandom2/randomStatetest507.json",
        "stRandom2/randomStatetest508.json",
        "stRandom2/randomStatetest509.json",
        "stRandom2/randomStatetest510.json",
        "stRandom2/randomStatetest511.json",
        "stRandom2/randomStatetest512.json",
        "stRandom2/randomStatetest514.json",
        "stRandom2/randomStatetest516.json",
        "stRandom2/randomStatetest517.json",
        "stRandom2/randomStatetest518.json",
        "stRandom2/randomStatetest519.json",
        "stRandom2/randomStatetest520.json",
        "stRandom2/randomStatetest521.json",
        "stRandom2/randomStatetest523.json",
        "stRandom2/randomStatetest524.json",
        "stRandom2/randomStatetest525.json",
        "stRandom2/randomStatetest526.json",
        "stRandom2/randomStatetest527.json",
        "stRandom2/randomStatetest528.json",
        "stRandom2/randomStatetest531.json",
        "stRandom2/randomStatetest532.json",
        "stRandom2/randomStatetest533.json",
        "stRandom2/randomStatetest534.json",
        "stRandom2/randomStatetest535.json",
        "stRandom2/randomStatetest536.json",
        "stRandom2/randomStatetest537.json",
        "stRandom2/randomStatetest539.json",
        "stRandom2/randomStatetest541.json",
        "stRandom2/randomStatetest542.json",
        "stRandom2/randomStatetest543.json",
        "stRandom2/randomStatetest545.json",
        "stRandom2/randomStatetest546.json",
        "stRandom2/randomStatetest547.json",
        "stRandom2/randomStatetest548.json",
        "stRandom2/randomStatetest550.json",
        "stRandom2/randomStatetest552.json",
        "stRandom2/randomStatetest553.json",
        "stRandom2/randomStatetest554.json",
        "stRandom2/randomStatetest555.json",
        "stRandom2/randomStatetest556.json",
        "stRandom2/randomStatetest558.json",
        "stRandom2/randomStatetest560.json",
        "stRandom2/randomStatetest562.json",
        "stRandom2/randomStatetest563.json",
        "stRandom2/randomStatetest564.json",
        "stRandom2/randomStatetest565.json",
        "stRandom2/randomStatetest566.json",
        "stRandom2/randomStatetest567.json",
        "stRandom2/randomStatetest569.json",
        "stRandom2/randomStatetest571.json",
        "stRandom2/randomStatetest574.json",
        "stRandom2/randomStatetest575.json",
        "stRandom2/randomStatetest576.json",
        "stRandom2/randomStatetest577.json",
        "stRandom2/randomStatetest578.json",
        "stRandom2/randomStatetest580.json",
        "stRandom2/randomStatetest582.json",
        "stRandom2/randomStatetest583.json",
        "stRandom2/randomStatetest584.json",
        "stRandom2/randomStatetest585.json",
        "stRandom2/randomStatetest586.json",
        "stRandom2/randomStatetest587.json",
        "stRandom2/randomStatetest588.json",
        "stRandom2/randomStatetest589.json",
        "stRandom2/randomStatetest592.json",
        "stRandom2/randomStatetest597.json",
        "stRandom2/randomStatetest599.json",
        "stRandom2/randomStatetest600.json",
        "stRandom2/randomStatetest601.json",
        "stRandom2/randomStatetest602.json",
        "stRandom2/randomStatetest603.json",
        "stRandom2/randomStatetest604.json",
        "stRandom2/randomStatetest605.json",
        "stRandom2/randomStatetest607.json",
        "stRandom2/randomStatetest608.json",
        "stRandom2/randomStatetest609.json",
        "stRandom2/randomStatetest610.json",
        "stRandom2/randomStatetest611.json",
        "stRandom2/randomStatetest612.json",
        "stRandom2/randomStatetest615.json",
        "stRandom2/randomStatetest616.json",
        "stRandom2/randomStatetest620.json",
        "stRandom2/randomStatetest621.json",
        "stRandom2/randomStatetest624.json",
        "stRandom2/randomStatetest625.json",
        "stRandom2/randomStatetest626.json",
        "stRandom2/randomStatetest628.json",
        "stRandom2/randomStatetest629.json",
        "stRandom2/randomStatetest630.json",
        "stRandom2/randomStatetest633.json",
        "stRandom2/randomStatetest636.json",
        "stRandom2/randomStatetest637.json",
        "stRandom2/randomStatetest638.json",
        "stRandom2/randomStatetest639.json",
        "stRandom2/randomStatetest640.json",
        "stRandom2/randomStatetest641.json",
        "stRevertTest/PythonRevertTestTue201814-1430.json",
        "stSolidityTest/CallInfiniteLoop.json",
        "stSolidityTest/CallRecursiveMethods.json",
        "stZeroKnowledge/ecmul_1-2_340282366920938463463374607431768211456_21000_128.json",
        "stZeroKnowledge/ecmul_1-2_340282366920938463463374607431768211456_21000_80.json",
        "stZeroKnowledge/ecmul_1-2_340282366920938463463374607431768211456_21000_96.json",
        "stZeroKnowledge/ecmul_1-2_5616_21000_128.json",
        "stZeroKnowledge/ecmul_1-2_5616_21000_96.json",
        "stZeroKnowledge/ecmul_1-2_5617_21000_128.json",
        "stZeroKnowledge/ecmul_1-2_5617_21000_96.json",
        "stZeroKnowledge/ecmul_1-2_9_21000_128.json",
        "stZeroKnowledge/ecmul_1-2_9_21000_96.json",
        "stZeroKnowledge/ecmul_1-2_9935_21000_128.json",
        "stZeroKnowledge/ecmul_1-2_9935_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_0_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_0_21000_64.json",
        "stZeroKnowledge/ecmul_1-3_0_21000_80.json",
        "stZeroKnowledge/ecmul_1-3_0_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_1_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_1_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_2_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_2_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_340282366920938463463374607431768211456_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_340282366920938463463374607431768211456_21000_80.json",
        "stZeroKnowledge/ecmul_1-3_340282366920938463463374607431768211456_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_5616_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_5616_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_5617_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_5617_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_9_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_9_21000_96.json",
        "stZeroKnowledge/ecmul_1-3_9935_21000_128.json",
        "stZeroKnowledge/ecmul_1-3_9935_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_0_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_0_21000_64.json",
        "stZeroKnowledge/ecmul_7827-6598_0_21000_80.json",
        "stZeroKnowledge/ecmul_7827-6598_0_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_1_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_1_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_1456_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_1456_21000_80.json",
        "stZeroKnowledge/ecmul_7827-6598_1456_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_2_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_2_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_5616_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_5616_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_5617_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_5617_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_9_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_9_21000_96.json",
        "stZeroKnowledge/ecmul_7827-6598_9935_21000_128.json",
        "stZeroKnowledge/ecmul_7827-6598_9935_21000_96.json",
        "stZeroKnowledge/ecpairing_empty_data_insufficient_gas.json",
        "stZeroKnowledge/ecpairing_empty_data.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_21000_0.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_21000_128.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_21000_192.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_21000_64.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_21000_80_Paris.json",
        "stZeroKnowledge2/ecadd_0-0_0-0_25000_128.json",
        "stZeroKnowledge2/ecadd_0-0_1-2_21000_128.json",
        "stZeroKnowledge2/ecadd_0-0_1-2_21000_192.json",
        "stZeroKnowledge2/ecadd_0-0_1-3_21000_128.json",
        "stZeroKnowledge2/ecadd_0-3_1-2_21000_128.json",
        "stZeroKnowledge2/ecadd_1-2_0-0_21000_128.json",
        "stZeroKnowledge2/ecadd_1-2_0-0_21000_192.json",
        "stZeroKnowledge2/ecadd_1-2_0-0_21000_64.json",
        "stZeroKnowledge2/ecadd_1-2_1-2_21000_128.json",
        "stZeroKnowledge2/ecadd_1-2_1-2_21000_192.json",
        "stZeroKnowledge2/ecadd_1-3_0-0_21000_80.json",
        "stZeroKnowledge2/ecadd_1145-3932_1145-4651_21000_192.json",
        "stZeroKnowledge2/ecadd_1145-3932_2969-1336_21000_128.json",
        "stZeroKnowledge2/ecadd_6-9_19274124-124124_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_0.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_40.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_64.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_80.json",
        "stZeroKnowledge2/ecmul_0-0_0_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_0_28000_96.json",
        "stZeroKnowledge2/ecmul_0-0_1_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_1_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_2_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_2_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_340282366920938463463374607431768211456_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_340282366920938463463374607431768211456_21000_80.json",
        "stZeroKnowledge2/ecmul_0-0_340282366920938463463374607431768211456_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_5616_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_5616_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_5617_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_5617_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_9_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_9_21000_96.json",
        "stZeroKnowledge2/ecmul_0-0_9935_21000_128.json",
        "stZeroKnowledge2/ecmul_0-0_9935_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_0_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_0_21000_64.json",
        "stZeroKnowledge2/ecmul_0-3_0_21000_80.json",
        "stZeroKnowledge2/ecmul_0-3_0_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_1_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_1_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_2_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_2_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_340282366920938463463374607431768211456_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_340282366920938463463374607431768211456_21000_80.json",
        "stZeroKnowledge2/ecmul_0-3_340282366920938463463374607431768211456_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_5616_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_5616_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_5617_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_5617_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_9_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_9_21000_96.json",
        "stZeroKnowledge2/ecmul_0-3_9935_21000_128.json",
        "stZeroKnowledge2/ecmul_0-3_9935_21000_96.json",
        "stZeroKnowledge2/ecmul_1-2_0_21000_128.json",
        "stZeroKnowledge2/ecmul_1-2_0_21000_64.json",
        "stZeroKnowledge2/ecmul_1-2_0_21000_80.json",
        "stZeroKnowledge2/ecmul_1-2_0_21000_96.json",
        "stZeroKnowledge2/ecmul_1-2_1_21000_128.json",
        "stZeroKnowledge2/ecmul_1-2_1_21000_96.json",
        "stZeroKnowledge2/ecmul_1-2_2_21000_128.json",
        "stZeroKnowledge2/ecmul_1-2_2_21000_96.json",
    ];
    for test in state_tests {
        let path = path_prefix.clone() + test;
        let raw_content = fs::read_to_string(Path::new(&path))
            .unwrap_or_else(|_| panic!("Cannot read suite: {:?}", test));
        let parsed_suite: smodels::TestSuite = serde_json::from_str(&raw_content)
            .unwrap_or_else(|_| panic!("Cannot parse suite: {:?}", test));
        for (_, unit) in parsed_suite.0 {
            run_test_unit(unit)
        }
    }
}
