//! `AnchorStateRegistry` contract bindings.
//!
//! Provides the anchor state (latest finalized output root) used as the starting
//! point when no pending dispute games exist.

use alloy_primitives::{Address, B256};
use alloy_provider::RootProvider;
use alloy_sol_types::sol;
use async_trait::async_trait;

use crate::ProposerError;

sol! {
    /// `AnchorStateRegistry` contract interface.
    #[sol(rpc)]
    interface IAnchorStateRegistry {
        /// Returns the current anchor root and its L2 sequence number.
        function getAnchorRoot() external view returns (bytes32 root, uint256 l2SequenceNumber);

        /// Returns the address of the `DisputeGameFactory`.
        function disputeGameFactory() external view returns (address);

        /// Returns the respected game type.
        function respectedGameType() external view returns (uint32);

        /// Returns whether a game is finalized.
        function isGameFinalized(address game) external view returns (bool);

        /// Returns whether a game is blacklisted.
        function isGameBlacklisted(address game) external view returns (bool);

        /// Returns whether a game is retired.
        function isGameRetired(address game) external view returns (bool);

        /// Returns whether a game is respected.
        function isGameRespected(address game) external view returns (bool);

        /// Returns whether the system is paused.
        function paused() external view returns (bool);
    }
}

/// Anchor root returned by `AnchorStateRegistry.getAnchorRoot()`.
#[derive(Debug, Clone)]
pub struct AnchorRoot {
    /// The output root hash.
    pub root: B256,
    /// The L2 block number (sequence number).
    pub l2_block_number: u64,
}

/// Async trait for reading anchor state.
#[async_trait]
pub trait AnchorStateRegistryClient: Send + Sync {
    /// Returns the current anchor root.
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ProposerError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct AnchorStateRegistryContractClient {
    contract: IAnchorStateRegistry::IAnchorStateRegistryInstance<RootProvider>,
}

impl AnchorStateRegistryContractClient {
    /// Creates a new client for the given contract address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ProposerError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IAnchorStateRegistry::IAnchorStateRegistryInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl AnchorStateRegistryClient for AnchorStateRegistryContractClient {
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ProposerError> {
        let result = self
            .contract
            .getAnchorRoot()
            .call()
            .await
            .map_err(|e| ProposerError::Contract(format!("getAnchorRoot failed: {e}")))?;

        let l2_block_number: u64 = result.l2SequenceNumber.try_into().map_err(|_| {
            ProposerError::Contract("anchor l2SequenceNumber overflows u64".to_string())
        })?;

        tracing::info!(
            root = ?result.root,
            l2_block_number,
            "Read anchor root from AnchorStateRegistry"
        );

        Ok(AnchorRoot { root: result.root, l2_block_number })
    }
}
