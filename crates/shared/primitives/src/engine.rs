//! Direct Engine API trait definitions.
//!
//! This module provides composable traits for Engine API functionality:
//!
//! - [`RollupConfigProvider`] - Access to rollup configuration
//! - [`BlockProvider`] - L1 and L2 block fetching
//! - [`ProofProvider`] - Account and storage proof retrieval
//! - [`LegacyPayloadSupport`] - Legacy v1 payload submission
//! - [`DirectEngineApi`] - Composed trait combining all of the above with [`OpEngineApi`]
//!
//! The trait composition follows the pattern established by kona's `EngineClient`
//! trait, enabling flexible implementation where components can be used independently
//! or together.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, Network, RpcWithBlock, network::Ethereum};
use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadStatus};
/// Response type for EIP-1186 account proofs.
///
/// Re-exported from `alloy_rpc_types_eth` for convenience.
pub use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_transport::TransportResult;
use kona_engine::HyperAuthClient;
use kona_genesis::RollupConfig;
use op_alloy_network::Optimism;
use op_alloy_provider::ext::engine::OpEngineApi;

/// A storage key for proof queries.
pub type StorageKey = B256;

// ============================================================================
// RollupConfigProvider
// ============================================================================

/// Provides access to the rollup configuration.
///
/// This trait enables components to access chain-specific rollup parameters
/// without requiring the full engine API.
///
/// # Example
///
/// ```ignore
/// use base_primitives::RollupConfigProvider;
///
/// fn check_chain_id<P: RollupConfigProvider>(provider: &P) {
///     let config = provider.cfg();
///     println!("L2 Chain ID: {}", config.l2_chain_id);
/// }
/// ```
pub trait RollupConfigProvider {
    /// Returns a reference to the [`RollupConfig`].
    ///
    /// The rollup config contains chain-specific parameters such as:
    /// - L1 chain ID and block time
    /// - L2 chain ID and genesis info
    /// - System config addresses
    /// - Fork activation timestamps
    fn cfg(&self) -> &RollupConfig;
}

// ============================================================================
// BlockProvider
// ============================================================================

/// Provides L1 and L2 block fetching capabilities.
///
/// This trait abstracts over the ability to query blocks from both the L1
/// (Ethereum) and L2 (Optimism) chains, which is essential for derivation
/// and cross-chain state verification.
///
/// # Example
///
/// ```ignore
/// use base_primitives::BlockProvider;
/// use alloy_eips::BlockId;
///
/// async fn get_latest_blocks<P: BlockProvider>(provider: &P) {
///     let l1_block = provider.get_l1_block(BlockId::latest()).await?;
///     let l2_block = provider.get_l2_block(BlockId::latest()).await?;
/// }
/// ```
pub trait BlockProvider {
    /// Fetches the L1 block with the provided [`BlockId`].
    ///
    /// This method queries the L1 chain for a specific block, which is useful
    /// for derivation and cross-chain state verification.
    ///
    /// # Arguments
    ///
    /// * `block` - The block identifier (number, hash, or tag)
    ///
    /// # Returns
    ///
    /// A future that resolves to the L1 block response.
    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse>;

    /// Fetches the L2 block with the provided [`BlockId`].
    ///
    /// This method queries the L2 execution layer for a specific block.
    ///
    /// # Arguments
    ///
    /// * `block` - The block identifier (number, hash, or tag)
    ///
    /// # Returns
    ///
    /// A future that resolves to the L2 block response.
    fn get_l2_block(&self, block: BlockId) -> EthGetBlock<<Optimism as Network>::BlockResponse>;
}

// ============================================================================
// ProofProvider
// ============================================================================

/// Provides account and storage proof retrieval capabilities.
///
/// This trait abstracts over the `eth_getProof` RPC method, enabling
/// verification of account state against block state roots.
///
/// # Example
///
/// ```ignore
/// use base_primitives::{ProofProvider, StorageKey};
/// use alloy_primitives::Address;
///
/// async fn verify_account<P: ProofProvider>(provider: &P, address: Address) {
///     let storage_keys: Vec<StorageKey> = vec![];
///     let proof = provider.get_proof(address, storage_keys).await?;
///     // Verify proof against known state root...
/// }
/// ```
pub trait ProofProvider {
    /// Gets the account and storage values of the specified account including merkle proofs.
    ///
    /// This call can be used to verify that account data has not been tampered with.
    /// The returned proof can be validated against the state root in a block header.
    ///
    /// # Arguments
    ///
    /// * `address` - The account address to query
    /// * `keys` - Storage slot keys to include in the proof
    ///
    /// # Returns
    ///
    /// An RPC call that resolves to the account proof response containing:
    /// - Account proof (list of RLP-encoded trie nodes)
    /// - Storage proofs for each requested key
    /// - Account balance, nonce, code hash, and storage root
    fn get_proof(
        &self,
        address: Address,
        keys: Vec<StorageKey>,
    ) -> RpcWithBlock<(Address, Vec<StorageKey>), EIP1186AccountProofResponse>;
}

// ============================================================================
// LegacyPayloadSupport
// ============================================================================

/// Provides legacy v1 payload submission support.
///
/// This trait enables submission of Paris (Merge) fork payloads to the
/// execution layer. While OP Stack chains typically use v2+ methods,
/// v1 support is retained for compatibility with older execution clients
/// or specific testing scenarios.
///
/// # Example
///
/// ```ignore
/// use base_primitives::LegacyPayloadSupport;
/// use alloy_rpc_types_engine::ExecutionPayloadV1;
///
/// async fn submit_legacy_payload<P: LegacyPayloadSupport>(
///     provider: &P,
///     payload: ExecutionPayloadV1,
/// ) {
///     let status = provider.new_payload_v1(payload).await?;
///     println!("Payload status: {:?}", status);
/// }
/// ```
pub trait LegacyPayloadSupport {
    /// Sends the given payload to the execution layer client (Paris fork, v1).
    ///
    /// This is the legacy payload submission method from the Paris (Merge) fork.
    /// For OP Stack chains, v2+ methods are typically used, but v1 support is
    /// retained for compatibility.
    ///
    /// # Arguments
    ///
    /// * `payload` - The execution payload to submit
    ///
    /// # Returns
    ///
    /// The payload validation status.
    fn new_payload_v1(
        &self,
        payload: ExecutionPayloadV1,
    ) -> impl std::future::Future<Output = TransportResult<PayloadStatus>> + Send;
}

// ============================================================================
// DirectEngineApi
// ============================================================================

/// Direct Engine API trait that composes all engine capabilities.
///
/// This trait combines:
/// - [`OpEngineApi`] - Core OP Stack Engine API methods (new_payload_v2/v3/v4,
///   fork_choice_updated_v2/v3, get_payload_v2/v3/v4, etc.)
/// - [`RollupConfigProvider`] - Access to rollup configuration
/// - [`BlockProvider`] - L1 and L2 block fetching
/// - [`ProofProvider`] - Account and storage proof retrieval
/// - [`LegacyPayloadSupport`] - Legacy v1 payload submission
///
/// # Example
///
/// ```ignore
/// use base_primitives::DirectEngineApi;
/// use alloy_eips::BlockId;
///
/// async fn example<E: DirectEngineApi>(engine: &E) {
///     // Access rollup config (from RollupConfigProvider)
///     let config = engine.cfg();
///
///     // Query L2 block (from BlockProvider)
///     let block = engine.get_l2_block(BlockId::latest()).await;
///
///     // Use inherited OpEngineApi methods
///     let status = engine.new_payload_v3(payload, parent_root).await;
/// }
/// ```
///
/// # Implementors
///
/// This trait is typically implemented by engine clients that need to interact
/// directly with an execution layer node, such as:
///
/// - Sequencer engines that build and seal blocks
/// - Derivation engines that process L1 data
/// - Validation engines that verify state transitions
pub trait DirectEngineApi:
    OpEngineApi<Optimism, alloy_transport_http::Http<HyperAuthClient>>
    + RollupConfigProvider
    + BlockProvider
    + ProofProvider
    + LegacyPayloadSupport
    + Send
    + Sync
{
}

// Blanket implementation for any type that implements all component traits
impl<T> DirectEngineApi for T where
    T: OpEngineApi<Optimism, alloy_transport_http::Http<HyperAuthClient>>
        + RollupConfigProvider
        + BlockProvider
        + ProofProvider
        + LegacyPayloadSupport
        + Send
        + Sync
{
}

// ============================================================================
// Compile-Time Trait Bound Assertions
// ============================================================================

/// Compile-time assertions verifying that [`DirectEngineApi`] implies all component traits.
///
/// This ensures that any type implementing `DirectEngineApi` automatically satisfies
/// all the individual trait bounds. If the trait hierarchy changes in a way that
/// breaks these relationships, compilation will fail.
#[allow(dead_code)]
const _: () = {
    /// Asserts that `T: DirectEngineApi` implies `T: RollupConfigProvider`.
    const fn assert_implies_rollup_config_provider<T: DirectEngineApi + RollupConfigProvider>() {}

    /// Asserts that `T: DirectEngineApi` implies `T: BlockProvider`.
    const fn assert_implies_block_provider<T: DirectEngineApi + BlockProvider>() {}

    /// Asserts that `T: DirectEngineApi` implies `T: ProofProvider`.
    const fn assert_implies_proof_provider<T: DirectEngineApi + ProofProvider>() {}

    /// Asserts that `T: DirectEngineApi` implies `T: LegacyPayloadSupport`.
    const fn assert_implies_legacy_payload_support<T: DirectEngineApi + LegacyPayloadSupport>() {}

    /// Asserts that `T: DirectEngineApi` implies `T: OpEngineApi`.
    const fn assert_implies_op_engine_api<
        T: DirectEngineApi + OpEngineApi<Optimism, alloy_transport_http::Http<HyperAuthClient>>,
    >() {
    }

    /// Asserts that `T: DirectEngineApi` implies `T: Send`.
    const fn assert_implies_send<T: DirectEngineApi + Send>() {}

    /// Asserts that `T: DirectEngineApi` implies `T: Sync`.
    const fn assert_implies_sync<T: DirectEngineApi + Sync>() {}

    /// Master assertion: verifies DirectEngineApi implies ALL component traits.
    ///
    /// If this function compiles, it proves that any `T: DirectEngineApi` must also
    /// implement all of the component traits listed in the bounds.
    const fn assert_direct_engine_api_implies_all<T: DirectEngineApi>() {
        assert_implies_rollup_config_provider::<T>();
        assert_implies_block_provider::<T>();
        assert_implies_proof_provider::<T>();
        assert_implies_legacy_payload_support::<T>();
        assert_implies_op_engine_api::<T>();
        assert_implies_send::<T>();
        assert_implies_sync::<T>();
    }
};
