//! `AnchorStateRegistry` contract bindings.
//!
//! Provides the anchor state (latest finalized output root) used as the starting
//! point when no pending dispute games exist.

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ContractError;

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

        /// Updates the anchor game to the given dispute game.
        ///
        /// Permissionless — anyone can call. The contract validates that the
        /// game is proper, respected, finalized, resolved as `DEFENDER_WINS`,
        /// and newer than the current anchor.
        function setAnchorState(address game) external;
    }
}

/// Encodes the calldata for `IAnchorStateRegistry.setAnchorState(game)`.
///
/// The transaction should be sent to the `AnchorStateRegistry` contract
/// address, passing the dispute game proxy address as the argument.
pub fn encode_set_anchor_state_calldata(game: Address) -> Bytes {
    let call = IAnchorStateRegistry::setAnchorStateCall { game };
    Bytes::from(call.abi_encode())
}

/// Anchor root returned by `AnchorStateRegistry.getAnchorRoot()`.
#[derive(Debug, Clone, Copy)]
pub struct AnchorRoot {
    /// The output root hash.
    pub root: B256,
    /// The L2 block number (sequence number).
    pub l2_block_number: u64,
}

/// Snapshot of `AnchorStateRegistry` state read in a single batch when
/// preparing a `setAnchorState()` call. Callers must already know the game
/// is finalized; this batch only covers the eligibility flags and the
/// current anchor root.
#[derive(Debug, Clone, Copy)]
pub struct AnchorPreflight {
    /// Whether the game is blacklisted (permanent failure).
    pub blacklisted: bool,
    /// Whether the game is retired (permanent failure).
    pub retired: bool,
    /// Whether the game currently matches the registry's respected game type.
    pub respected: bool,
    /// The current anchor root in the registry.
    pub anchor_root: AnchorRoot,
}

impl AnchorPreflight {
    /// Returns `true` if the game can never become a valid anchor and the
    /// caller should stop retrying `setAnchorState()` for it.
    pub const fn permanently_ineligible(&self) -> bool {
        self.blacklisted || self.retired
    }
}

/// Async trait for reading anchor state.
#[async_trait]
pub trait AnchorStateRegistryClient: Send + Sync {
    /// Returns the current anchor root.
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ContractError>;
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct AnchorStateRegistryContractClient {
    contract: IAnchorStateRegistry::IAnchorStateRegistryInstance<RootProvider>,
}

impl AnchorStateRegistryContractClient {
    /// Creates a new client for the given contract address and L1 RPC URL.
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Result<Self, ContractError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IAnchorStateRegistry::IAnchorStateRegistryInstance::new(address, provider);
        Ok(Self { contract })
    }
}

#[async_trait]
impl AnchorStateRegistryClient for AnchorStateRegistryContractClient {
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ContractError> {
        let result = self.contract.getAnchorRoot().call().await.map_err(|e| {
            ContractError::Call { context: "getAnchorRoot failed".into(), source: e }
        })?;

        let l2_block_number: u64 = result.l2SequenceNumber.try_into().map_err(|_| {
            ContractError::Validation("anchor l2SequenceNumber overflows u64".into())
        })?;

        tracing::info!(
            root = ?result.root,
            l2_block_number,
            "Read anchor root from AnchorStateRegistry"
        );

        Ok(AnchorRoot { root: result.root, l2_block_number })
    }
}
