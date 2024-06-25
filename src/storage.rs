use std::{fmt::Debug, sync::Arc};

use ahash::AHashMap;
use alloy_primitives::{Address, Bytes, B256, U256};
use bitvec::vec::BitVec;
use revm::{
    db::PlainAccount,
    interpreter::analysis::to_analysed,
    primitives::{Account, AccountInfo, Bytecode, JumpTable},
    DatabaseRef,
};
use serde::{Deserialize, Serialize};

/// An EVM account.
// TODO: Flatten [AccountBasic] or more ideally, replace this with an Alloy type.
// [AccountBasic] works for now as we're tightly tied to REVM types, hence
// conversions between [AccountBasic] & [AccountInfo] are very convenient.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvmAccount {
    /// The account's basic information.
    pub basic: AccountBasic,
    /// The account's storage.
    pub storage: AHashMap<U256, U256>,
}

impl From<PlainAccount> for EvmAccount {
    fn from(account: PlainAccount) -> Self {
        EvmAccount {
            basic: account.info.into(),
            storage: account.storage.into_iter().collect(),
        }
    }
}

impl From<Account> for EvmAccount {
    fn from(account: Account) -> Self {
        Self {
            basic: account.info.into(),
            storage: account
                .storage
                .iter()
                .map(|(k, v)| (*k, v.present_value))
                .collect(),
        }
    }
}

impl From<AccountBasic> for EvmAccount {
    fn from(basic: AccountBasic) -> Self {
        EvmAccount {
            basic,
            storage: AHashMap::default(),
        }
    }
}

/// Basic information of an account
// TODO: Reuse something sane from Alloy?
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AccountBasic {
    /// The balance of the account.
    pub balance: U256,
    /// The nonce of the account.
    pub nonce: u64,
    /// The code of the account.
    pub code: Option<EvmCode>,
    /// The optional code hash to avoid rehashing during execution
    pub code_hash: Option<B256>,
}

impl AccountBasic {
    /// Create a new account with the given balance, nonce, code hash and code.
    pub fn new(balance: U256, nonce: u64, code_hash: B256, code: EvmCode) -> Self {
        AccountBasic {
            balance,
            nonce,
            code: Some(code),
            code_hash: Some(code_hash),
        }
    }

    /// Check if an account is empty for removal per EIP-161
    // https://github.com/ethereum/EIPs/blob/96523ef4d76ca440f73f0403ddb5c9cb3b24dcae/EIPS/eip-161.md
    pub fn is_empty(&self) -> bool {
        self.balance == U256::ZERO && self.nonce == 0 && self.code.is_none()
    }

    /// Create a new account with the given balance.
    pub fn from_balance(balance: U256) -> Self {
        AccountBasic {
            balance,
            ..Default::default()
        }
    }
}

impl From<AccountBasic> for AccountInfo {
    fn from(account: AccountBasic) -> Self {
        let code = account.code.map(Bytecode::from).unwrap_or_default();
        AccountInfo::new(
            account.balance,
            account.nonce,
            account.code_hash.unwrap_or_else(|| code.hash_slow()),
            code,
        )
    }
}

impl From<AccountInfo> for AccountBasic {
    fn from(account: AccountInfo) -> Self {
        let code = account.code.and_then(|code| {
            if code.is_empty() {
                None
            } else {
                Some(code.into())
            }
        });
        AccountBasic {
            balance: account.balance,
            nonce: account.nonce,
            // Currently trust the account info instead of rehashing.
            code_hash: code.as_ref().map(|_| account.code_hash),
            code,
        }
    }
}

/// EVM Code, currently mapping to REVM's [ByteCode::LegacyAnalyzed].
// TODO: Support raw legacy & EOF
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EvmCode {
    /// Bytecode with 32 zero bytes padding
    bytecode: Bytes,
    /// Original bytes length
    original_len: usize,
    /// Jump table.
    jump_table: Arc<BitVec<u8>>,
}

impl From<EvmCode> for Bytecode {
    fn from(code: EvmCode) -> Self {
        // TODO: Better error handling.
        // A common trap would be converting a default [EvmCode] into
        // a [Bytecode]. On failure we should fallback to legacy and
        // analyse again.
        unsafe {
            Bytecode::new_analyzed(code.bytecode, code.original_len, JumpTable(code.jump_table))
        }
    }
}

impl From<Bytecode> for EvmCode {
    fn from(code: Bytecode) -> Self {
        match code {
            Bytecode::LegacyRaw(_) => to_analysed(code).into(),
            Bytecode::LegacyAnalyzed(code) => EvmCode {
                bytecode: code.bytecode,
                original_len: code.original_len,
                jump_table: code.jump_table.0,
            },
            Bytecode::Eof(_) => unimplemented!("TODO: Support EOF"),
        }
    }
}

/// An interface to provide chain state to Pevm for transaction execution.
/// Staying close to the underlying REVM's Database trait while not leaking
/// its primitives to library users (favoring Alloy at the moment).
/// TODO: Better API for third-party integration.
pub trait Storage {
    /// Errors when querying data from storage.
    type Error: Debug;

    /// Get basic account information.
    fn basic(&self, address: &Address) -> Result<Option<AccountBasic>, Self::Error>;

    /// Check if an address is a contract.
    /// This default implementation is slow as it clones the account basic via
    /// the [basic] call. Performant storages should re-implement it with an
    /// internal reference check.
    fn is_contract(&self, address: &Address) -> Result<bool, Self::Error> {
        self.basic(address)
            .map(|account| account.is_some_and(|account| account.code.is_some()))
    }

    /// Get account code by its hash.
    fn code_by_hash(&self, code_hash: &B256) -> Result<Option<EvmCode>, Self::Error>;

    /// Get if the account already has storage (to support EIP-7610).
    fn has_storage(&self, address: &Address) -> Result<bool, Self::Error>;

    /// Get storage value of address at index.
    fn storage(&self, address: &Address, index: &U256) -> Result<U256, Self::Error>;

    /// Get block hash by block number.
    fn block_hash(&self, number: &U256) -> Result<B256, Self::Error>;
}

// We can use any REVM database as storage provider. Convenient for
// testing blocks fetched from RPC via REVM's [CachedDB]. Otherwise, use
// our [Storage] types to avoid redundant conversions.
// TODO: Do something equivalent to [CachedDB] ourselves and remove this.
impl<D: DatabaseRef> Storage for D
where
    D::Error: Debug,
{
    type Error = D::Error;

    fn basic(&self, address: &Address) -> Result<Option<AccountBasic>, Self::Error> {
        D::basic_ref(self, *address).map(|a| a.map(|a| a.into()))
    }

    fn code_by_hash(&self, code_hash: &B256) -> Result<Option<EvmCode>, Self::Error> {
        D::code_by_hash_ref(self, *code_hash).map(|bytecode| {
            if bytecode.is_empty() {
                None
            } else {
                Some(EvmCode::from(bytecode))
            }
        })
    }

    fn has_storage(&self, address: &Address) -> Result<bool, Self::Error> {
        D::has_storage_ref(self, *address)
    }

    fn storage(&self, address: &Address, index: &U256) -> Result<U256, Self::Error> {
        D::storage_ref(self, *address, *index)
    }

    fn block_hash(&self, number: &U256) -> Result<B256, Self::Error> {
        D::block_hash_ref(self, *number)
    }
}

// We want to use our [Storage] as REVM's [DatabaseRef] to provide data for
// things like sequential execution fallback.
pub(crate) struct StorageWrapper<S: Storage>(pub(crate) S);

impl<S: Storage> DatabaseRef for StorageWrapper<S> {
    type Error = S::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        S::basic(&self.0, &address).map(|account| account.map(AccountBasic::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        S::code_by_hash(&self.0, &code_hash)
            .map(|code| code.map(Bytecode::from).unwrap_or_default())
    }

    fn has_storage_ref(&self, address: Address) -> Result<bool, Self::Error> {
        S::has_storage(&self.0, &address)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        S::storage(&self.0, &address, &index)
    }

    fn block_hash_ref(&self, number: U256) -> Result<B256, Self::Error> {
        S::block_hash(&self.0, &number)
    }
}

mod in_memory;
pub use in_memory::InMemoryStorage;
mod rpc;
pub use rpc::RpcStorage;
