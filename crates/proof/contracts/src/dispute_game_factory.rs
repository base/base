//! `DisputeGameFactory` contract bindings.
//!
//! Used to create new dispute games and query existing ones.

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, SolError, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// `DisputeGameFactory` contract interface.
    #[sol(rpc)]
    interface IDisputeGameFactory {
        /// Error returned when a game with the same UUID already exists.
        error GameAlreadyExists(bytes32 uuid);

        /// Creates a new dispute game with proof data passed to `initializeWithInitData`.
        function createWithInitData(
            uint32 gameType,
            bytes32 rootClaim,
            bytes calldata extraData,
            bytes calldata initData
        ) external payable returns (address proxy);

        /// Returns the game at the given index.
        function gameAtIndex(uint256 index) external view returns (
            uint32 gameType,
            uint64 timestamp,
            address proxy
        );

        /// Returns the total number of games.
        function gameCount() external view returns (uint256);

        /// Returns the bond required to create a game of the given type.
        function initBonds(uint32 gameType) external view returns (uint256);

        /// Returns the implementation address for the given game type.
        function gameImpls(uint32 gameType) external view returns (address);

        /// Looks up a game by its unique `(gameType, rootClaim, extraData)` tuple.
        ///
        /// Returns `address(0)` when no matching game exists.
        function games(
            uint32 gameType,
            bytes32 rootClaim,
            bytes calldata extraData
        ) external view returns (address proxy, uint64 timestamp);
    }
}

/// Information about a game at a factory index.
#[derive(Debug, Clone, Copy)]
pub struct GameAtIndex {
    /// The game type ID.
    pub game_type: u32,
    /// The creation timestamp.
    pub timestamp: u64,
    /// The proxy address of the game contract.
    pub proxy: Address,
}

/// Async trait for interacting with the `DisputeGameFactory`.
#[async_trait]
pub trait DisputeGameFactoryClient: Send + Sync {
    /// Returns the total number of games created.
    async fn game_count(&self) -> Result<u64, ContractError>;

    /// Returns the game at the given factory index.
    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError>;

    /// Returns the bond required to create a game of the given type.
    async fn init_bonds(&self, game_type: u32) -> Result<U256, ContractError>;

    /// Returns the implementation address for the given game type.
    async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError>;

    /// Looks up a game by its unique `(gameType, rootClaim, extraData)` tuple.
    ///
    /// Returns `Address::ZERO` when no matching game exists.
    async fn games(
        &self,
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
    ) -> Result<Address, ContractError>;
}

/// The 4-byte selector for `GameAlreadyExists(bytes32)`.
pub const fn game_already_exists_selector() -> [u8; 4] {
    IDisputeGameFactory::GameAlreadyExists::SELECTOR
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct DisputeGameFactoryContractClient {
    contract: IDisputeGameFactory::IDisputeGameFactoryInstance<RootProvider>,
}

impl DisputeGameFactoryContractClient {
    /// Creates a new client for the given contract address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ContractError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IDisputeGameFactory::IDisputeGameFactoryInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl DisputeGameFactoryClient for DisputeGameFactoryContractClient {
    async fn game_count(&self) -> Result<u64, ContractError> {
        let result = contract_call!(self.contract.gameCount().call(), "gameCount failed")?;

        result.try_into().map_err(|_| ContractError::validation("gameCount overflows u64"))
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ContractError> {
        let result = contract_call!(
            self.contract.gameAtIndex(U256::from(index)).call(),
            format!("gameAtIndex({index}) failed")
        )?;

        Ok(GameAtIndex {
            game_type: result.gameType,
            timestamp: result.timestamp,
            proxy: result.proxy,
        })
    }

    async fn init_bonds(&self, game_type: u32) -> Result<U256, ContractError> {
        let result = contract_call!(self.contract.initBonds(game_type).call(), "initBonds failed")?;

        Ok(result)
    }

    async fn game_impls(&self, game_type: u32) -> Result<Address, ContractError> {
        let result = contract_call!(self.contract.gameImpls(game_type).call(), "gameImpls failed")?;

        Ok(result)
    }

    async fn games(
        &self,
        game_type: u32,
        root_claim: B256,
        extra_data: Bytes,
    ) -> Result<Address, ContractError> {
        let result = contract_call!(
            self.contract.games(game_type, root_claim, extra_data).call(),
            "games lookup failed"
        )?;

        Ok(result.proxy)
    }
}

/// Encodes the `extraData` for `DisputeGameFactory.createWithInitData()`.
///
/// Format: `l2BlockNumber(32) + parentAddress(20) + intermediateRoots(32 * N)`.
///
/// This is **packed encoding** (not ABI-encoded). The Solidity contract
/// reads these fields at fixed byte offsets via clone-with-immutable-args
/// (CWIA).
pub fn encode_extra_data(
    l2_block_number: u64,
    parent_address: Address,
    intermediate_roots: &[B256],
) -> Bytes {
    let mut data = vec![0u8; 52 + 32 * intermediate_roots.len()];
    data[..32].copy_from_slice(&U256::from(l2_block_number).to_be_bytes::<32>());
    data[32..52].copy_from_slice(parent_address.as_slice());
    for (i, root) in intermediate_roots.iter().enumerate() {
        data[52 + i * 32..52 + (i + 1) * 32].copy_from_slice(root.as_slice());
    }
    Bytes::from(data)
}

/// Values used to look up a dispute game by UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameLookupKey {
    /// L2 block number encoded into `extraData`.
    pub target_block: u64,
    /// Final output root used as the factory `rootClaim`.
    pub root_claim: B256,
    /// Packed factory `extraData`.
    pub extra_data: Bytes,
}

/// Error while building a dispute game lookup key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GameLookupError {
    /// `BLOCK_INTERVAL` must not be zero.
    #[error("block_interval must not be zero")]
    ZeroBlockInterval,
    /// `INTERMEDIATE_BLOCK_INTERVAL` must not be zero.
    #[error("intermediate_block_interval must not be zero")]
    ZeroIntermediateBlockInterval,
    /// `BLOCK_INTERVAL` must be divisible by `INTERMEDIATE_BLOCK_INTERVAL`.
    #[error(
        "block_interval {block_interval} is not divisible by intermediate_block_interval {intermediate_block_interval}"
    )]
    InvalidInterval {
        /// Number of L2 blocks between games.
        block_interval: u64,
        /// Number of L2 blocks between intermediate roots.
        intermediate_block_interval: u64,
    },
    /// Arithmetic overflow while computing checkpoint blocks.
    #[error("overflow computing game lookup block")]
    BlockOverflow,
    /// The supplied roots must cover every checkpoint, including the final root.
    #[error("intermediate root count mismatch: expected {expected}, got {actual}")]
    IntermediateRootCount {
        /// Expected root count.
        expected: usize,
        /// Actual root count.
        actual: usize,
    },
}

/// Returns the number of roots needed to build a game UUID lookup key.
pub fn game_lookup_count(
    block_interval: u64,
    intermediate_block_interval: u64,
) -> Result<usize, GameLookupError> {
    if block_interval == 0 {
        return Err(GameLookupError::ZeroBlockInterval);
    }
    if intermediate_block_interval == 0 {
        return Err(GameLookupError::ZeroIntermediateBlockInterval);
    }
    if !block_interval.is_multiple_of(intermediate_block_interval) {
        return Err(GameLookupError::InvalidInterval {
            block_interval,
            intermediate_block_interval,
        });
    }

    usize::try_from(block_interval / intermediate_block_interval)
        .map_err(|_| GameLookupError::BlockOverflow)
}

/// Returns the checkpoint blocks used by a game UUID lookup.
pub fn game_lookup_blocks(
    starting_block_number: u64,
    block_interval: u64,
    intermediate_block_interval: u64,
) -> Result<Vec<u64>, GameLookupError> {
    let count = game_lookup_count(block_interval, intermediate_block_interval)?;

    (1..=count)
        .map(|i| {
            let multiplier = u64::try_from(i).map_err(|_| GameLookupError::BlockOverflow)?;
            let offset = intermediate_block_interval
                .checked_mul(multiplier)
                .ok_or(GameLookupError::BlockOverflow)?;
            starting_block_number.checked_add(offset).ok_or(GameLookupError::BlockOverflow)
        })
        .collect()
}

/// Builds the key used by `DisputeGameFactory.games()`.
pub fn game_lookup_key(
    starting_block_number: u64,
    parent_address: Address,
    block_interval: u64,
    intermediate_block_interval: u64,
    intermediate_roots: &[B256],
) -> Result<GameLookupKey, GameLookupError> {
    let expected = game_lookup_count(block_interval, intermediate_block_interval)?;
    if intermediate_roots.len() != expected {
        return Err(GameLookupError::IntermediateRootCount {
            expected,
            actual: intermediate_roots.len(),
        });
    }

    let target_block =
        starting_block_number.checked_add(block_interval).ok_or(GameLookupError::BlockOverflow)?;
    let root_claim = intermediate_roots[expected - 1];
    let extra_data = encode_extra_data(target_block, parent_address, intermediate_roots);

    Ok(GameLookupKey { target_block, root_claim, extra_data })
}

/// Encodes the calldata for `DisputeGameFactory.createWithInitData()`.
pub fn encode_create_calldata(
    game_type: u32,
    root_claim: B256,
    extra_data: Bytes,
    init_data: Bytes,
) -> Bytes {
    let call = IDisputeGameFactory::createWithInitDataCall {
        gameType: game_type,
        rootClaim: root_claim,
        extraData: extra_data,
        initData: init_data,
    };
    Bytes::from(call.abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_extra_data() {
        let parent = Address::repeat_byte(0x42);
        let data = encode_extra_data(1000, parent, &[]);
        assert_eq!(data.len(), 52);

        assert_eq!(&data[24..32], &1000u64.to_be_bytes());
        assert_eq!(&data[32..52], parent.as_slice());
    }

    #[test]
    fn test_encode_extra_data_no_parent() {
        let registry = Address::repeat_byte(0xAA);
        let data = encode_extra_data(500, registry, &[]);
        assert_eq!(&data[32..52], registry.as_slice());
    }

    #[test]
    fn test_encode_extra_data_with_intermediate_roots() {
        let parent = Address::repeat_byte(0x42);
        let roots = vec![B256::repeat_byte(0xAA), B256::repeat_byte(0xBB)];
        let data = encode_extra_data(1000, parent, &roots);
        assert_eq!(data.len(), 52 + 64);

        assert_eq!(&data[24..32], &1000u64.to_be_bytes());
        assert_eq!(&data[32..52], parent.as_slice());
        assert_eq!(&data[52..84], roots[0].as_slice());
        assert_eq!(&data[84..116], roots[1].as_slice());
    }

    #[test]
    fn test_encode_create_calldata_has_selector() {
        let calldata = encode_create_calldata(
            1,
            B256::ZERO,
            Bytes::from(vec![0u8; 36]),
            Bytes::from(vec![0u8; 130]),
        );
        assert_eq!(&calldata[..4], &IDisputeGameFactory::createWithInitDataCall::SELECTOR);
    }

    #[test]
    fn test_game_already_exists_selector() {
        let selector = game_already_exists_selector();
        assert_eq!(selector.len(), 4);
        // Just verify we get a non-zero selector
        assert_ne!(selector, [0u8; 4]);
    }

    #[test]
    fn test_game_lookup_blocks() {
        let blocks = game_lookup_blocks(100, 30, 10).unwrap();

        assert_eq!(blocks, vec![110, 120, 130]);
    }

    #[test]
    fn test_game_lookup_blocks_rejects_bad_intervals() {
        assert_eq!(game_lookup_blocks(100, 0, 10).unwrap_err(), GameLookupError::ZeroBlockInterval);
        assert_eq!(
            game_lookup_blocks(100, 30, 0).unwrap_err(),
            GameLookupError::ZeroIntermediateBlockInterval
        );
        assert_eq!(
            game_lookup_blocks(100, 30, 20).unwrap_err(),
            GameLookupError::InvalidInterval {
                block_interval: 30,
                intermediate_block_interval: 20
            }
        );
    }

    #[test]
    fn test_game_lookup_key() {
        let parent = Address::repeat_byte(0x42);
        let roots = vec![B256::repeat_byte(0xAA), B256::repeat_byte(0xBB)];

        let key = game_lookup_key(100, parent, 20, 10, &roots).unwrap();

        assert_eq!(key.target_block, 120);
        assert_eq!(key.root_claim, roots[1]);
        assert_eq!(key.extra_data, encode_extra_data(120, parent, &roots));
    }

    #[test]
    fn test_game_lookup_key_rejects_wrong_root_count() {
        let err = game_lookup_key(100, Address::ZERO, 20, 10, &[B256::ZERO]).unwrap_err();

        assert_eq!(err, GameLookupError::IntermediateRootCount { expected: 2, actual: 1 });
    }
}
