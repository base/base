//! Direct Engine API trait definitions.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_provider::{EthGetBlock, Network, RpcWithBlock, network::Ethereum};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2,
    ExecutionPayloadV1, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
/// Response type for EIP-1186 account proofs.
pub use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use kona_genesis::RollupConfig;
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes,
};
use thiserror::Error;

/// A storage key for proof queries.
pub type StorageKey = B256;

// ============================================================================
// EngineApiError
// ============================================================================

/// Engine API client result type.
pub type EngineApiResult<T> = Result<T, EngineApiError>;

/// Engine API error type.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct EngineApiError {
    /// The error message.
    pub message: String,
}

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
pub trait RollupConfigProvider {
    /// Returns a reference to the [`RollupConfig`].
    fn cfg(&self) -> &RollupConfig;
}

// ============================================================================
// BlockProvider
// ============================================================================

/// Provides L1 and L2 block fetching capabilities.
pub trait BlockProvider {
    /// Fetches the L1 block with the provided [`BlockId`].
    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse>;

    /// Fetches the L2 block with the provided [`BlockId`].
    fn get_l2_block(&self, block: BlockId) -> EthGetBlock<<Optimism as Network>::BlockResponse>;
}

// ============================================================================
// ProofProvider
// ============================================================================

/// Provides account and storage proof retrieval via `eth_getProof`.
pub trait ProofProvider {
    /// Gets the account and storage values of the specified account including merkle proofs.
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
pub trait LegacyPayloadSupport: Send + Sync {
    /// Sends the given payload to the execution layer client (v1).
    fn new_payload_v1(
        &self,
        payload: ExecutionPayloadV1,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;
}

// ============================================================================
// EngineApiClient
// ============================================================================

/// Transport-agnostic Engine API client trait.
pub trait EngineApiClient: Send + Sync {
    /// Sends the given payload to the execution layer client (v2).
    fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Sends the given payload to the execution layer client (v3).
    fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        parent_beacon_block_root: B256,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Sends the given payload to the execution layer client (v4).
    fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        parent_beacon_block_root: B256,
    ) -> impl std::future::Future<Output = EngineApiResult<PayloadStatus>> + Send;

    /// Updates the fork choice state (v2).
    fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> impl std::future::Future<Output = EngineApiResult<ForkchoiceUpdated>> + Send;

    /// Updates the fork choice state (v3).
    fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> impl std::future::Future<Output = EngineApiResult<ForkchoiceUpdated>> + Send;

    /// Retrieves an execution payload from a build process (v2).
    fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadEnvelopeV2>> + Send;

    /// Retrieves an execution payload from a build process (v3).
    fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<OpExecutionPayloadEnvelopeV3>> + Send;

    /// Retrieves an execution payload from a build process (v4).
    fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> impl std::future::Future<Output = EngineApiResult<OpExecutionPayloadEnvelopeV4>> + Send;

    /// Returns the execution payload bodies by hash.
    fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<B256>,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadBodiesV1>> + Send;

    /// Returns the execution payload bodies by range.
    fn get_payload_bodies_by_range_v1(
        &self,
        start: u64,
        count: u64,
    ) -> impl std::future::Future<Output = EngineApiResult<ExecutionPayloadBodiesV1>> + Send;

    /// Returns the execution client version.
    fn get_client_version_v1(
        &self,
        client_version: ClientVersionV1,
    ) -> impl std::future::Future<Output = EngineApiResult<Vec<ClientVersionV1>>> + Send;

    /// Exchanges supported Engine API capabilities.
    fn exchange_capabilities(
        &self,
        capabilities: Vec<String>,
    ) -> impl std::future::Future<Output = EngineApiResult<Vec<String>>> + Send;
}

// ============================================================================
// DirectEngineApi
// ============================================================================

/// Composed trait combining all engine capabilities.
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

/// Compile-time assertions verifying [`DirectEngineApi`] implies all component traits.
#[allow(dead_code)]
const _: () = {
    const fn assert_implies_rollup_config_provider<T: DirectEngineApi + RollupConfigProvider>() {}
    const fn assert_implies_block_provider<T: DirectEngineApi + BlockProvider>() {}
    const fn assert_implies_proof_provider<T: DirectEngineApi + ProofProvider>() {}
    const fn assert_implies_legacy_payload_support<T: DirectEngineApi + LegacyPayloadSupport>() {}
    const fn assert_implies_engine_api_client<T: DirectEngineApi + EngineApiClient>() {}
    const fn assert_implies_send<T: DirectEngineApi + Send>() {}
    const fn assert_implies_sync<T: DirectEngineApi + Sync>() {}

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
