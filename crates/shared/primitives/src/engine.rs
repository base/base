//! Direct Engine API trait definitions.
//!
//! This module provides composable traits for Engine API functionality:
//!
//! - [`RollupConfigProvider`] - Access to rollup configuration
//! - [`BlockProvider`] - L1 and L2 block fetching
//! - [`ProofProvider`] - Account and storage proof retrieval
//! - [`LegacyPayloadSupport`] - Legacy v1 payload submission
//! - [`DirectEngineApi`] - Composed trait combining all of the above with [`EngineApiClient`]
//!
//! The trait composition follows the pattern established by kona's `EngineClient`
//! trait, enabling flexible implementation where components can be used independently
//! or together.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, Network, RpcWithBlock, network::Ethereum};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2,
    ExecutionPayloadV1, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
/// Response type for EIP-1186 account proofs.
///
/// Re-exported from `alloy_rpc_types_eth` for convenience.
pub use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use kona_genesis::RollupConfig;
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes, ProtocolVersion,
};

/// A storage key for proof queries.
pub type StorageKey = B256;

// ============================================================================
// EngineApiError
// ============================================================================

/// Engine API client result type.
pub type EngineApiResult<T> = Result<T, EngineApiError>;

/// Engine API error type.
///
/// This is a transport-agnostic error type that can wrap errors from different
/// transport implementations (HTTP, in-process channels, etc.).
#[derive(Debug, Clone)]
pub struct EngineApiError {
    /// The error message.
    pub message: String,
}

impl std::fmt::Display for EngineApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for EngineApiError {}

impl EngineApiError {
    /// Creates a new engine API error.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

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
pub trait LegacyPayloadSupport: Send + Sync {
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
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;
}

// ============================================================================
// EngineApiClient
// ============================================================================

/// Transport-agnostic Engine API client trait.
///
/// This trait provides the core Engine API methods without requiring a specific
/// transport implementation. It mirrors [`op_alloy_provider::ext::engine::OpEngineApi`]
/// but uses a generic error type instead of `TransportResult`.
///
/// # Example
///
/// ```ignore
/// use base_primitives::EngineApiClient;
///
/// async fn submit_payload<E: EngineApiClient>(engine: &E, payload: ExecutionPayloadV3) {
///     let status = engine.new_payload_v3(payload, parent_root).await?;
///     println!("Payload status: {:?}", status);
/// }
/// ```
pub trait EngineApiClient: Send + Sync {
    /// Sends the given payload to the execution layer client (Engine API v2).
    fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Sends the given payload to the execution layer client (Engine API v3).
    ///
    /// For OP Stack chains, versioned hashes are always empty.
    fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        parent_beacon_block_root: B256,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Sends the given payload to the execution layer client (Engine API v4).
    fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        parent_beacon_block_root: B256,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Updates the fork choice state (Engine API v2).
    fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> impl std::future::Future<Output = EngineApiResult<ForkchoiceUpdated>> + Send;

    /// Updates the fork choice state (Engine API v3).
    fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> impl std::future::Future<Output = EngineApiResult<ForkchoiceUpdated>> + Send;

    /// Retrieves an execution payload from a previously started build process (v2).
    fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadEnvelopeV2>> + Send;

    /// Retrieves an execution payload from a previously started build process (v3).
    fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<OpExecutionPayloadEnvelopeV3>> + Send;

    /// Retrieves an execution payload from a previously started build process (v4).
    fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<OpExecutionPayloadEnvelopeV4>> + Send;

    /// Returns the execution payload bodies by the given hash.
    fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<B256>,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadBodiesV1>> + Send;

    /// Returns the execution payload bodies by the range starting at `start`, containing `count`
    /// blocks.
    fn get_payload_bodies_by_range_v1(
        &self,
        start: u64,
        count: u64,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadBodiesV1>> + Send;

    /// Returns the execution client version information.
    fn get_client_version_v1(
        &self,
        client_version: ClientVersionV1,
    ) -> impl std::future::Future<Output = EngineApiResult<Vec<ClientVersionV1>>> + Send;

    /// Signals superchain information to the Engine.
    fn signal_superchain_v1(
        &self,
        recommended: ProtocolVersion,
        required: ProtocolVersion,
    ) -> impl std::future::Future<Output = EngineApiResult<ProtocolVersion>> + Send;

    /// Returns the list of Engine API methods supported by the execution layer client software.
    fn exchange_capabilities(
        &self,
        capabilities: Vec<String>,
    ) -> impl std::future::Future<Output = EngineApiResult<Vec<String>>> + Send;
}

// ============================================================================
// DirectEngineApi
// ============================================================================

/// Direct Engine API trait that composes all engine capabilities.
///
/// This trait combines:
/// - [`EngineApiClient`] - Core Engine API methods (new_payload, fork_choice_updated,
///   get_payload, etc.)
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
///     // Use EngineApiClient methods
///     let status = engine.new_payload_v3(payload, parent_root).await?;
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
    EngineApiClient + RollupConfigProvider + BlockProvider + ProofProvider + LegacyPayloadSupport
{
}

// Blanket implementation for any type that implements all component traits
impl<T> DirectEngineApi for T where
    T: EngineApiClient
        + RollupConfigProvider
        + BlockProvider
        + ProofProvider
        + LegacyPayloadSupport
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

    /// Asserts that `T: DirectEngineApi` implies `T: EngineApiClient`.
    const fn assert_implies_engine_api_client<T: DirectEngineApi + EngineApiClient>() {}

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
        assert_implies_engine_api_client::<T>();
        assert_implies_send::<T>();
        assert_implies_sync::<T>();
    }
};
