use crate::common::storage::{
    from_address, from_indices, from_short_string, from_tick, StorageBuilder,
};
use ahash::AHashMap;
use pevm::{AccountBasic, EvmAccount};
use revm::primitives::{
    fixed_bytes,
    hex::{FromHex, ToHexExt},
    keccak256, uint, Address, Bytecode, Bytes, FixedBytes, B256, U256,
};

const POOL_FEE: u32 = 3000;
const TICK_SPACING: i32 = 60;

const SINGLE_SWAP: &str = include_str!("./assets/SingleSwap.hex");
const UNISWAP_V3_FACTORY: &str = include_str!("./assets/UniswapV3Factory.hex");
const WETH9: &str = include_str!("./assets/WETH9.hex");
const UNISWAP_V3_POOL: &str = include_str!("./assets/UniswapV3Pool.hex");
const SWAP_ROUTER: &str = include_str!("./assets/SwapRouter.hex");

#[inline]
fn keccak256_all(chunks: &[&[u8]]) -> B256 {
    keccak256(chunks.concat())
}

// @gnosis/canonical-weth/contracts/WETH9.sol
#[derive(Debug, Default)]
pub struct WETH9 {}

impl WETH9 {
    pub fn new() -> Self {
        Self {}
    }
    // | Name      | Type                                            | Slot | Offset | Bytes |
    // |-----------|-------------------------------------------------|------|--------|-------|
    // | name      | string                                          | 0    | 0      | 32    |
    // | symbol    | string                                          | 1    | 0      | 32    |
    // | decimals  | uint8                                           | 2    | 0      | 1     |
    // | balanceOf | mapping(address => uint256)                     | 3    | 0      | 32    |
    // | allowance | mapping(address => mapping(address => uint256)) | 4    | 0      | 32    |
    pub fn build(&self) -> EvmAccount {
        let hex = WETH9.trim();
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(0, from_short_string("Wrapped Ether"));
        store.set(1, from_short_string("WETH"));
        store.set(3, 18);
        store.set(4, 0); // mapping
        store.set(5, 0); // mapping

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
}

// @uniswap/v3-core/contracts/UniswapV3Factory.sol
#[derive(Debug, Default)]
pub struct UniswapV3Factory {
    owner: Address,
    pools: AHashMap<(Address, Address, U256), Address>,
}

impl UniswapV3Factory {
    pub fn new(owner: Address) -> Self {
        Self {
            owner,
            pools: AHashMap::new(),
        }
    }

    pub fn add_pool(
        &mut self,
        token_0: Address,
        token_1: Address,
        pool_address: Address,
    ) -> &mut Self {
        self.pools
            .insert((token_0, token_1, U256::from(POOL_FEE)), pool_address);
        self
    }

    // | Name                 | Type                                                               | Slot | Offset | Bytes |
    // |----------------------|--------------------------------------------------------------------|------|--------|-------|
    // | parameters           | struct UniswapV3PoolDeployer.Parameters                            | 0    | 0      | 96    |
    // | owner                | address                                                            | 3    | 0      | 20    |
    // | feeAmountTickSpacing | mapping(uint24 => int24)                                           | 4    | 0      | 32    |
    // | getPool              | mapping(address => mapping(address => mapping(uint24 => address))) | 5    | 0      | 32    |
    pub fn build(&self, address: Address) -> EvmAccount {
        let hex = UNISWAP_V3_FACTORY.trim().replace(
            "0b748751e6f8b1a38c9386a19d9f8966b3593a9e",
            &address.encode_hex(),
        );
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(0, 0);
        store.set(1, 0);
        store.set(2, 0);
        store.set(3, from_address(self.owner));
        store.set(4, 0); // mapping
        store.set(5, 0); // mapping

        store.set(from_indices(4, &[500]), 10);
        store.set(from_indices(4, &[3000]), 60);
        store.set(from_indices(4, &[10000]), 200);

        for ((token_0, token_1, pool_fee), pool_address) in self.pools.iter() {
            store.set(
                from_indices(
                    5,
                    &[from_address(*token_0), from_address(*token_1), *pool_fee],
                ),
                from_address(*pool_address),
            );
            store.set(
                from_indices(
                    5,
                    &[from_address(*token_1), from_address(*token_0), *pool_fee],
                ),
                from_address(*pool_address),
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
}

// @uniswap/v3-core/contracts/UniswapV3Pool.sol
#[derive(Debug, Default)]
pub struct UniswapV3Pool {
    token_0: Address,
    token_1: Address,
    factory: Address,
    positions: AHashMap<U256, [U256; 4]>,
    ticks: AHashMap<U256, [U256; 4]>,
    tick_bitmap: AHashMap<U256, U256>,
}

impl UniswapV3Pool {
    pub fn new(token_0: Address, token_1: Address, factory: Address) -> Self {
        Self {
            token_0,
            token_1,
            factory,
            positions: AHashMap::new(),
            ticks: AHashMap::new(),
            tick_bitmap: AHashMap::new(),
        }
    }

    pub fn add_position(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        value: [U256; 4],
    ) -> &mut Self {
        let t0 = &FixedBytes::<20>::from(owner)[..]; // 20 bytes
        let t1 = &FixedBytes::<32>::from(from_tick(tick_lower))[29..32]; // 3 bytes
        let t2 = &FixedBytes::<32>::from(from_tick(tick_upper))[29..32]; // 3 bytes
        let key: U256 = keccak256([t0, t1, t2].concat()).into();
        self.positions.insert(key, value);
        self
    }

    pub fn add_tick(&mut self, tick: i32, value: [U256; 4]) -> &mut Self {
        self.ticks.insert(from_tick(tick), value);

        let index: i32 = tick / TICK_SPACING;
        self.tick_bitmap
            .entry(from_tick(index >> 8))
            .or_default()
            .set_bit((index & 0xff).try_into().unwrap(), true);
        self
    }

    // | Name                 | Type                                     | Slot | Offset | Bytes   |
    // |----------------------|------------------------------------------|------|--------|---------|
    // | slot0                | struct UniswapV3Pool.Slot0               | 0    | 0      | 32      |
    // | feeGrowthGlobal0X128 | uint256                                  | 1    | 0      | 32      |
    // | feeGrowthGlobal1X128 | uint256                                  | 2    | 0      | 32      |
    // | protocolFees         | struct UniswapV3Pool.ProtocolFees        | 3    | 0      | 32      |
    // | liquidity            | uint128                                  | 4    | 0      | 16      |
    // | ticks                | mapping(int24 => struct Tick.Info)       | 5    | 0      | 32      |
    // | tickBitmap           | mapping(int16 => uint256)                | 6    | 0      | 32      |
    // | positions            | mapping(bytes32 => struct Position.Info) | 7    | 0      | 32      |
    // | observations         | struct Oracle.Observation[65535]         | 8    | 0      | 2097120 |
    pub fn build(&self, address: Address) -> EvmAccount {
        let hex = UNISWAP_V3_POOL
            .trim()
            .replace(
                "261d8c5e9742e6f7f1076fa1f560894524e19cad",
                &self.token_0.encode_hex(),
            )
            .replace(
                "ce3478a9e0167a6bc5716dc39dbbbfac38f27623",
                &self.token_1.encode_hex(),
            )
            .replace(
                "cba6b9a951749b8735c603e7ffc5151849248772",
                &self.factory.encode_hex(),
            )
            .replace(
                "d495d5e5cab2567777fff988c4fcd71328b17c9d",
                &address.encode_hex(),
            );
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(
            0,
            uint!(0x0001000001000100000000000000000000000001000000000000000000000000_U256),
        );
        store.set(1, 0);
        store.set(2, 0);
        store.set(3, 0);
        store.set(4, 111_111_000_000_010_412_955_141u128);
        store.set(5, 0); // mapping
        store.set(6, 0); // mapping
        store.set(7, 0); // mapping
        store.set(
            8,
            uint!(0x0100000000000000000000000000000000000000000000000000000000000001_U256),
        );

        for (key, value) in self.ticks.iter() {
            store.set_many(from_indices(5, &[*key]), value);
        }

        for (key, value) in self.tick_bitmap.iter() {
            store.set(from_indices(6, &[*key]), *value);
        }

        for (key, value) in self.positions.iter() {
            store.set_many(from_indices(7, &[*key]), value);
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

    // @uniswap/v3-periphery/contracts/libraries/PoolAddress.sol
    pub fn get_address(&self, factory_address: Address, pool_init_code_hash: B256) -> Address {
        let hash = keccak256_all(&[
            fixed_bytes!("ff").as_slice(),
            factory_address.as_slice(),
            keccak256_all(&[
                B256::left_padding_from(self.token_0.as_slice()).as_slice(),
                B256::left_padding_from(self.token_1.as_slice()).as_slice(),
                B256::from(U256::from(POOL_FEE)).as_slice(),
            ])
            .as_slice(),
            pool_init_code_hash.as_slice(),
        ]);
        Address::from_slice(&hash[12..32])
    }
}

// @uniswap/v3-periphery/contracts/SwapRouter.sol
#[derive(Debug, Default)]
pub struct SwapRouter {
    weth9: Address,
    factory: Address,
    pool_init_code_hash: B256,
}

impl SwapRouter {
    pub fn new(weth9: Address, factory: Address, pool_init_code_hash: B256) -> Self {
        Self {
            weth9,
            factory,
            pool_init_code_hash,
        }
    }

    // | Name           | Type    | Slot | Offset | Bytes |
    // |----------------|---------|------|--------|-------|
    // | amountInCached | uint256 | 0    | 0      | 32    |
    pub fn build(&self) -> EvmAccount {
        let hex = SWAP_ROUTER
            .trim()
            .replace(
                "6509f2a854ba7441039fce3b959d5badd2ffcfcd",
                &self.weth9.encode_hex(),
            )
            .replace(
                "d787a42ee3ac477c46dd6c912e7af795d44453d5",
                &self.factory.encode_hex(),
            )
            .replace(
                "636fcf59d7fdf3a03833dba1c6d936c9d7c6730057c8c4c8e5059feaeab60e04",
                &self.pool_init_code_hash.encode_hex(),
            );

        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(
            0,
            uint!(0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff_U256),
        );

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
}

/// `@risechain/op-test-bench/foundry/src/SingleSwap.sol`
#[derive(Debug, Default)]
pub struct SingleSwap {
    swap_router: Address,
    token_0: Address,
    token_1: Address,
}

impl SingleSwap {
    pub fn new(swap_router: Address, token_0: Address, token_1: Address) -> Self {
        Self {
            swap_router,
            token_0,
            token_1,
        }
    }

    // | Name   | Type    | Slot | Offset | Bytes |
    // |--------|---------|------|--------|-------|
    // | token0 | address | 0    | 0      | 20    |
    // | token1 | address | 1    | 0      | 20    |
    // | fee    | uint24  | 1    | 20     | 3     |
    pub fn build(&self) -> EvmAccount {
        let hex = SINGLE_SWAP.trim().replace(
            "e7cfcccb38ce07ba9d8d13431afe8cf6172de031",
            &self.swap_router.encode_hex(),
        );
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());

        let mut store = StorageBuilder::new();
        store.set(0, from_address(self.token_0));
        store.set(1, from_address(self.token_1));
        store.set_with_offset(1, 20, 3, POOL_FEE);

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
}
