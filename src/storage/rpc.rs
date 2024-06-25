use std::{fmt::Debug, future::IntoFuture, sync::Mutex};

use ahash::AHashMap;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockId, BlockNumberOrTag};
use alloy_transport::TransportError;
use alloy_transport_http::Http;
use reqwest::Client;
use revm::{
    db::PlainAccount,
    precompile::{PrecompileSpecId, Precompiles},
    primitives::{AccountInfo, Bytecode, SpecId},
};
use tokio::runtime::Runtime;

use crate::EvmAccount;

// TODO: Support generic network & transport types.
// TODO: Put this behind an RPC flag to not pollute the core
// library with RPC network & transport dependencies, etc.
type RpcProvider = RootProvider<Http<Client>>;

/// A storage that fetches state data via RPC for execution.
#[derive(Debug)]
pub struct RpcStorage {
    provider: RpcProvider,
    block_id: BlockId,
    precompiles: &'static Precompiles,
    // Convenient types for persisting then reconstructing block's state
    // as in-memory storage for benchmarks & testing. Also work well when
    // the storage is re-used, like for comparing sequential & parallel
    // execution on the same block.
    // Using a `Mutex` so we don't propagate mutability requirements back
    // to our `Storage` trait and meet `Send`/`Sync` requirements for PEVM.
    cache_accounts: Mutex<AHashMap<Address, EvmAccount>>,
    cache_block_hashes: Mutex<AHashMap<U256, B256>>,
    // TODO: Better async handling.
    runtime: Runtime,
}

impl RpcStorage {
    /// Create a new RPC Storage
    pub fn new(provider: RpcProvider, spec_id: SpecId, block_id: BlockId) -> Self {
        RpcStorage {
            provider,
            precompiles: Precompiles::new(PrecompileSpecId::from_spec_id(spec_id)),
            block_id,
            cache_accounts: Mutex::default(),
            cache_block_hashes: Mutex::default(),
            // TODO: Better error handling.
            runtime: Runtime::new().unwrap(),
        }
    }

    /// Get a snapshot of accounts
    pub fn get_cache_accounts(&self) -> AHashMap<Address, EvmAccount> {
        self.cache_accounts.lock().unwrap().clone()
    }

    /// Get a snapshot of block hashes
    pub fn get_cache_block_hashes(&self) -> AHashMap<U256, B256> {
        self.cache_block_hashes.lock().unwrap().clone()
    }
}

// TODO: Implement [Storage] instead.
// Going with Revm's [Database] simply to make it easier
// to try matching sequential & parallel execution.
// In the future we should match block roots anyway.
impl revm::DatabaseRef for RpcStorage {
    type Error = TransportError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.cache_accounts.lock().unwrap().get(&address) {
            return Ok(Some(AccountInfo::from(account.basic.clone())));
        }
        self.runtime.block_on(async {
            let (res_balance, res_nonce, res_code) = tokio::join!(
                self.provider
                    .get_balance(address)
                    .block_id(self.block_id)
                    .into_future(),
                self.provider
                    .get_transaction_count(address)
                    .block_id(self.block_id)
                    .into_future(),
                self.provider
                    .get_code_at(address)
                    .block_id(self.block_id)
                    .into_future()
            );
            let balance = res_balance?;
            let nonce = res_nonce?;
            let code = res_code?;
            // We need to distinguish new non-precompile accounts for gas calculation
            // in early hard-forks (creating new accounts cost extra gas, etc.).
            if !self
                .precompiles
                .addresses()
                .any(|precompile_address| precompile_address == &address)
                && balance.is_zero()
                && nonce == 0
                && code.is_empty()
            {
                return Ok(None);
            }
            let code = Bytecode::new_raw(code);
            let info = AccountInfo::new(balance, nonce, code.hash_slow(), code);
            let plain_account = PlainAccount::from(info.clone());
            self.cache_accounts
                .lock()
                .unwrap()
                .insert(address, plain_account.into());
            Ok(Some(info))
        })
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        panic!("This should not be called as the code is already loaded via account");
    }

    fn has_storage_ref(&self, _address: Address) -> Result<bool, Self::Error> {
        // FIXME! Returning [false] should cover EIP-7610 for the time being.
        Ok(false)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(account) = self.cache_accounts.lock().unwrap().get(&address) {
            if let Some(value) = account.storage.get(&index) {
                return Ok(*value);
            }
        }
        let value = self.runtime.block_on(
            self.provider
                .get_storage_at(address, index)
                .block_id(self.block_id)
                .into_future(),
        )?;
        match self.cache_accounts.lock().unwrap().entry(address) {
            std::collections::hash_map::Entry::Occupied(mut account) => {
                account.get_mut().storage.insert(index, value);
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(EvmAccount {
                    basic: self.basic_ref(address)?.unwrap_or_default().into(),
                    storage: [(index, value)].into_iter().collect(),
                });
            }
        };
        Ok(value)
    }

    fn block_hash_ref(&self, number: U256) -> Result<B256, Self::Error> {
        if let Some(&block_hash) = self.cache_block_hashes.lock().unwrap().get(&number) {
            return Ok(block_hash);
        }

        let block_hash = self
            .runtime
            .block_on(
                self.provider
                    .get_block_by_number(BlockNumberOrTag::Number(number.to::<u64>()), false)
                    .into_future(),
            )
            .map(|block| block.unwrap().header.hash.unwrap())?;

        self.cache_block_hashes
            .lock()
            .unwrap()
            .insert(number, block_hash);

        Ok(block_hash)
    }
}
