//! `DisputeGameFactory` contract bindings.
//!
//! Used to create new dispute games and query existing ones.

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ProposerError;

sol! {
    /// `DisputeGameFactory` contract interface.
    #[sol(rpc)]
    interface IDisputeGameFactory {
        /// Error returned when a game with the same UUID already exists.
        error GameAlreadyExists(bytes32 uuid);

        /// Creates a new dispute game.
        function create(
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
    }
}

/// Information about a game at a factory index.
#[derive(Debug, Clone)]
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
    async fn game_count(&self) -> Result<u64, ProposerError>;

    /// Returns the game at the given factory index.
    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ProposerError>;

    /// Returns the bond required to create a game of the given type.
    async fn init_bonds(&self, game_type: u32) -> Result<U256, ProposerError>;

    /// Returns the implementation address for the given game type.
    async fn game_impls(&self, game_type: u32) -> Result<Address, ProposerError>;
}

/// The 4-byte selector for `GameAlreadyExists(bytes32)`.
pub const fn game_already_exists_selector() -> [u8; 4] {
    use alloy_sol_types::SolError;
    IDisputeGameFactory::GameAlreadyExists::SELECTOR
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[allow(missing_debug_implementations)]
pub struct DisputeGameFactoryContractClient {
    contract: IDisputeGameFactory::IDisputeGameFactoryInstance<RootProvider>,
}

impl DisputeGameFactoryContractClient {
    /// Creates a new client for the given contract address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ProposerError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IDisputeGameFactory::IDisputeGameFactoryInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl DisputeGameFactoryClient for DisputeGameFactoryContractClient {
    async fn game_count(&self) -> Result<u64, ProposerError> {
        let result = self
            .contract
            .gameCount()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("gameCount failed: {e}")))?;

        result
            .try_into()
            .map_err(|_| ProposerError::Contract("gameCount overflows u64".to_string()))
    }

    async fn game_at_index(&self, index: u64) -> Result<GameAtIndex, ProposerError> {
        let result =
            self.contract.gameAtIndex(U256::from(index)).call().await.map_err(|e| {
                ProposerError::Contract(format!("gameAtIndex({index}) failed: {e}"))
            })?;

        Ok(GameAtIndex {
            game_type: result.gameType,
            timestamp: result.timestamp,
            proxy: result.proxy,
        })
    }

    async fn init_bonds(&self, game_type: u32) -> Result<U256, ProposerError> {
        let result = self
            .contract
            .initBonds(game_type)
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("initBonds failed: {e}")))?;

        Ok(result)
    }

    async fn game_impls(&self, game_type: u32) -> Result<Address, ProposerError> {
        let result = self
            .contract
            .gameImpls(game_type)
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("gameImpls failed: {e}")))?;

        Ok(result)
    }
}

/// Encodes the `extraData` for `DisputeGameFactory.create()`.
///
/// Format: `l2BlockNumber(32) + parentIndex(4) + intermediateRoots(32 * N)`.
pub fn encode_extra_data(
    l2_block_number: u64,
    parent_index: u32,
    intermediate_roots: &[B256],
) -> Bytes {
    let mut data = vec![0u8; 36 + 32 * intermediate_roots.len()];
    U256::from(l2_block_number)
        .to_be_bytes::<32>()
        .iter()
        .enumerate()
        .for_each(|(i, b)| data[i] = *b);
    data[32..36].copy_from_slice(&parent_index.to_be_bytes());
    for (i, root) in intermediate_roots.iter().enumerate() {
        data[36 + i * 32..36 + (i + 1) * 32].copy_from_slice(root.as_slice());
    }
    Bytes::from(data)
}

/// Encodes the calldata for `DisputeGameFactory.create()`.
pub fn encode_create_calldata(
    game_type: u32,
    root_claim: B256,
    extra_data: Bytes,
    init_data: Bytes,
) -> Bytes {
    let call = IDisputeGameFactory::createCall {
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
        let data = encode_extra_data(1000, 42, &[]);
        assert_eq!(data.len(), 36);

        assert_eq!(&data[24..32], &1000u64.to_be_bytes());
        assert_eq!(&data[32..36], &42u32.to_be_bytes());
    }

    #[test]
    fn test_encode_extra_data_no_parent() {
        let data = encode_extra_data(500, 0xFFFFFFFF, &[]);
        assert_eq!(&data[32..36], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_encode_extra_data_with_intermediate_roots() {
        let roots = vec![B256::repeat_byte(0xAA), B256::repeat_byte(0xBB)];
        let data = encode_extra_data(1000, 42, &roots);
        assert_eq!(data.len(), 36 + 64);

        assert_eq!(&data[24..32], &1000u64.to_be_bytes());
        assert_eq!(&data[32..36], &42u32.to_be_bytes());
        assert_eq!(&data[36..68], roots[0].as_slice());
        assert_eq!(&data[68..100], roots[1].as_slice());
    }

    #[test]
    fn test_encode_create_calldata_has_selector() {
        let calldata = encode_create_calldata(
            1,
            B256::ZERO,
            Bytes::from(vec![0u8; 36]),
            Bytes::from(vec![0u8; 130]),
        );
        // First 4 bytes should be the create function selector
        assert_eq!(&calldata[..4], &IDisputeGameFactory::createCall::SELECTOR);
    }

    #[test]
    fn test_game_already_exists_selector() {
        let selector = game_already_exists_selector();
        assert_eq!(selector.len(), 4);
        // Just verify we get a non-zero selector
        assert_ne!(selector, [0u8; 4]);
    }
}
