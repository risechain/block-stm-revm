use crate::common::storage::{from_address, from_indices, from_short_string, StorageBuilder};
use ahash::AHashMap;
use pevm::{AccountBasic, EvmAccount};
use revm::primitives::{
    fixed_bytes, hex::FromHex, ruint::UintTryFrom, Address, Bytecode, Bytes, B256, U256,
};

const ERC20_TOKEN: &str = include_str!("./assets/ERC20Token.hex");

// @risechain/op-test-bench/foundry/src/ERC20Token.sol
#[derive(Debug, Default)]
pub struct ERC20Token {
    name: String,
    symbol: String,
    decimals: U256,
    initial_supply: U256,
    balances: AHashMap<Address, U256>,
    allowances: AHashMap<(Address, Address), U256>,
}

impl ERC20Token {
    pub fn new<U, V>(name: &str, symbol: &str, decimals: U, initial_supply: V) -> Self
    where
        U256: UintTryFrom<U>,
        U256: UintTryFrom<V>,
    {
        Self {
            name: String::from(name),
            symbol: String::from(symbol),
            decimals: U256::from(decimals),
            initial_supply: U256::from(initial_supply),
            balances: AHashMap::new(),
            allowances: AHashMap::new(),
        }
    }

    pub fn add_balances(&mut self, addresses: &[Address], amount: U256) -> &mut Self {
        for address in addresses {
            self.balances.insert(*address, amount);
        }
        self
    }

    pub fn add_allowances(
        &mut self,
        addresses: &[Address],
        spender: Address,
        amount: U256,
    ) -> &mut Self {
        for address in addresses {
            self.allowances.insert((*address, spender), amount);
        }
        self
    }

    // | Name         | Type                                            | Slot | Offset | Bytes |
    // |--------------|-------------------------------------------------|------|--------|-------|
    // | _balances    | mapping(address => uint256)                     | 0    | 0      | 32    |
    // | _allowances  | mapping(address => mapping(address => uint256)) | 1    | 0      | 32    |
    // | _totalSupply | uint256                                         | 2    | 0      | 32    |
    // | _name        | string                                          | 3    | 0      | 32    |
    // | _symbol      | string                                          | 4    | 0      | 32    |
    // | _decimals    | uint8                                           | 5    | 0      | 1     |
    pub fn build(&self) -> EvmAccount {
        let hex = ERC20_TOKEN.trim();
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(0, 0); // mapping
        store.set(1, 0); // mapping
        store.set(2, self.initial_supply);
        store.set(3, from_short_string(&self.name));
        store.set(4, from_short_string(&self.symbol));
        store.set(5, self.decimals);

        for (address, amount) in self.balances.iter() {
            store.set(from_indices(0, &[from_address(*address)]), *amount);
        }

        for ((address, spender), amount) in self.allowances.iter() {
            store.set(
                from_indices(1, &[from_address(*address), from_address(*spender)]),
                *amount,
            );
        }

        EvmAccount {
            basic: AccountBasic::new(
                U256::ZERO,
                1u64,
                bytecode.hash_slow(),
                bytecode.clone().into(),
            ),
            storage: store.build().into_iter().collect(),
        }
    }

    // $ forge inspect ERC20Token methods
    // {
    //   "allowance(address,address)": "dd62ed3e",
    //   "approve(address,uint256)": "095ea7b3",
    //   "balanceOf(address)": "70a08231",
    //   "decimals()": "313ce567",
    //   "decreaseAllowance(address,uint256)": "a457c2d7",
    //   "increaseAllowance(address,uint256)": "39509351",
    //   "name()": "06fdde03",
    //   "symbol()": "95d89b41",
    //   "totalSupply()": "18160ddd",
    //   "transfer(address,uint256)": "a9059cbb",
    //   "transferFrom(address,address,uint256)": "23b872dd"
    // }
    pub fn transfer(recipient: Address, amount: U256) -> Bytes {
        Bytes::from(
            [
                &fixed_bytes!("a9059cbb")[..],
                &B256::from(from_address(recipient))[..],
                &B256::from(amount)[..],
            ]
            .concat(),
        )
    }
}
