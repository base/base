//! `AggregateVerifier` contract bindings.
//!
//! Used to query individual dispute game instances and read the
//! `BLOCK_INTERVAL` from the implementation contract at startup.

use alloy::primitives::{Address, B256, U256};
use alloy::providers::RootProvider;
use alloy::sol;
use async_trait::async_trait;

use crate::ProposerError;

sol! {
    /// `AggregateVerifier` (dispute game) contract interface.
    ///
    /// Each game instance is a clone created by `DisputeGameFactory.create()`.
    #[sol(rpc)]
    interface IAggregateVerifier {
        /// Returns the root claim (output root) of this game.
        function rootClaim() external pure returns (bytes32);

        /// Returns the L2 block number this game proposes.
        function l2SequenceNumber() external pure returns (uint256);

        /// Returns the current game status (0=IN_PROGRESS, 1=CHALLENGER_WINS, 2=DEFENDER_WINS).
        function status() external view returns (uint8);

        /// Returns the address that provided a TEE proof.
        function teeProver() external view returns (address);

        /// Returns the address that provided a ZK proof.
        function zkProver() external view returns (address);

        /// Returns the parent game's factory index.
        function parentIndex() external pure returns (uint32);

        /// Returns the block interval between proposals (immutable on the implementation).
        function BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the intermediate block interval for intermediate output root checkpoints.
        function INTERMEDIATE_BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the game type.
        function gameType() external view returns (uint32);

        /// Returns the starting block number.
        function startingBlockNumber() external view returns (uint256);
    }
}

/// Information about a dispute game instance.
#[derive(Debug, Clone)]
pub struct GameInfo {
    /// The output root claimed by this game.
    pub root_claim: B256,
    /// The L2 block number of this game.
    pub l2_block_number: u64,
    /// The parent game's factory index.
    pub parent_index: u32,
}

/// Async trait for querying `AggregateVerifier` game instances.
#[async_trait]
pub trait AggregateVerifierClient: Send + Sync {
    /// Queries game details from a game proxy address.
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ProposerError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[allow(missing_debug_implementations)]
pub struct AggregateVerifierContractClient {
    provider: RootProvider,
}

impl AggregateVerifierContractClient {
    /// Creates a new client connected to the given L1 RPC URL.
    pub fn new(l1_rpc_url: url::Url) -> Result<Self, ProposerError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        Ok(Self { provider })
    }

    /// Reads `BLOCK_INTERVAL` from the `AggregateVerifier` implementation contract.
    pub async fn read_block_interval(&self, impl_address: Address) -> Result<u64, ProposerError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(impl_address, &self.provider);
        let interval_u256: U256 = contract
            .BLOCK_INTERVAL()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("BLOCK_INTERVAL failed: {e}")))?;

        let interval: u64 = interval_u256
            .try_into()
            .map_err(|_| ProposerError::Contract("BLOCK_INTERVAL overflows u64".to_string()))?;

        if interval < 2 {
            return Err(ProposerError::Contract(
                "BLOCK_INTERVAL must be at least 2 (single-block proposals are not supported)"
                    .to_string(),
            ));
        }

        Ok(interval)
    }

    /// Reads `INTERMEDIATE_BLOCK_INTERVAL` from the `AggregateVerifier` implementation contract.
    pub async fn read_intermediate_block_interval(
        &self,
        impl_address: Address,
    ) -> Result<u64, ProposerError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(impl_address, &self.provider);
        let interval_u256: U256 = contract
            .INTERMEDIATE_BLOCK_INTERVAL()
            .call()
            .await
            .map_err(|e| {
                ProposerError::Contract(format!("INTERMEDIATE_BLOCK_INTERVAL failed: {e}"))
            })?;

        let interval: u64 = interval_u256.try_into().map_err(|_| {
            ProposerError::Contract("INTERMEDIATE_BLOCK_INTERVAL overflows u64".to_string())
        })?;

        if interval == 0 {
            return Err(ProposerError::Contract(
                "INTERMEDIATE_BLOCK_INTERVAL cannot be 0".to_string(),
            ));
        }

        Ok(interval)
    }
}

#[async_trait]
impl AggregateVerifierClient for AggregateVerifierContractClient {
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ProposerError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let root_claim: B256 = contract
            .rootClaim()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("rootClaim failed: {e}")))?;

        let l2_seq: U256 = contract
            .l2SequenceNumber()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("l2SequenceNumber failed: {e}")))?;

        let l2_block_number: u64 = l2_seq
            .try_into()
            .map_err(|_| ProposerError::Contract("l2SequenceNumber overflows u64".to_string()))?;

        let parent_index: u32 = contract
            .parentIndex()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("parentIndex failed: {e}")))?;

        Ok(GameInfo {
            root_claim,
            l2_block_number,
            parent_index,
        })
    }
}
