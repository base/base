//! Transport-agnostic engine driver trait.

use alloy_eips::eip7685::Requests;
use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use kona_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadV4,
    OpPayloadAttributes,
};

/// Transport-agnostic engine API trait.
///
/// This trait mirrors the core Engine API operations from kona's [`EngineClient`] but without
/// the HTTP-specific bounds. This allows implementations to use any transport mechanism:
///
/// - HTTP (via kona's `OpEngineClient`)
/// - In-process (via [`InProcessEngineClient`][crate::InProcessEngineClient] communicating
///   directly with reth)
///
/// # Why This Exists
///
/// Kona's `EngineClient` trait is defined as:
/// ```ignore
/// pub trait EngineClient: OpEngineApi<Optimism, Http<HyperAuthClient>> + Send + Sync { ... }
/// ```
///
/// The `Http<HyperAuthClient>` bound is baked into the trait, making it impossible to
/// substitute an in-process implementation. This trait removes that coupling.
///
/// # Reference
///
/// See kona's EngineClient:
/// <https://github.com/op-rs/kona/blob/3c02e71e76206c5ec5980f8d3af62b123baab3fc/crates/node/engine/src/client.rs#L67>
#[async_trait]
pub trait DirectEngineApi: Send + Sync + 'static {
    /// The error type returned by engine operations.
    type Error: std::error::Error + Send + Sync + 'static;

    // ========================================================================
    // New Payload Methods (Insert payloads into the execution layer)
    // ========================================================================

    /// Sends a new payload to the execution layer (Engine API v1).
    ///
    /// Used for pre-Shanghai payloads.
    async fn new_payload_v1(
        &self,
        payload: ExecutionPayloadV1,
    ) -> Result<PayloadStatus, Self::Error>;

    /// Sends a new payload to the execution layer (Engine API v2).
    ///
    /// Used for Shanghai+ payloads with withdrawals.
    async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> Result<PayloadStatus, Self::Error>;

    /// Sends a new payload to the execution layer (Engine API v3).
    ///
    /// Used for Cancun+ payloads with blob versioned hashes.
    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, Self::Error>;

    /// Sends a new payload to the execution layer (Engine API v4 - OP-specific).
    ///
    /// Used for Prague+ payloads with execution requests.
    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Result<PayloadStatus, Self::Error>;

    // ========================================================================
    // Fork Choice Methods (Update canonical chain head)
    // ========================================================================

    /// Updates the fork choice state and optionally requests a new payload (v2).
    ///
    /// Used for pre-Ecotone fork choice updates.
    async fn fork_choice_updated_v2(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error>;

    /// Updates the fork choice state and optionally requests a new payload (v3).
    ///
    /// Used for Ecotone+ fork choice updates.
    async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, Self::Error>;

    // ========================================================================
    // Get Payload Methods (Retrieve built payloads)
    // ========================================================================

    /// Retrieves a payload by ID (Engine API v2).
    ///
    /// Used for pre-Ecotone payloads. Returns the standard Ethereum V2 envelope
    /// since OP Stack uses the same format for V2.
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV2, Self::Error>;

    /// Retrieves a payload by ID (Engine API v3).
    ///
    /// Used for Ecotone+ payloads.
    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV3, Self::Error>;

    /// Retrieves a payload by ID (Engine API v4 - OP-specific).
    ///
    /// Used for Isthmus+ payloads with execution requests.
    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<OpExecutionPayloadEnvelopeV4, Self::Error>;

    // ========================================================================
    // Block Query Methods (Fetch L2 block information)
    // ========================================================================

    /// Fetches L2 block info by block number.
    ///
    /// Returns `None` if the block is not found.
    async fn l2_block_info_by_number(
        &self,
        number: u64,
    ) -> Result<Option<L2BlockInfo>, Self::Error>;

    /// Fetches L2 block info for the latest block.
    ///
    /// Returns `None` if the chain is empty.
    async fn l2_block_info_latest(&self) -> Result<Option<L2BlockInfo>, Self::Error>;

    /// Fetches L2 block info by block hash.
    ///
    /// Returns `None` if the block is not found.
    async fn l2_block_info_by_hash(&self, hash: B256) -> Result<Option<L2BlockInfo>, Self::Error>;

    /// Fetches the safe head block info.
    async fn l2_safe_head(&self) -> Result<Option<L2BlockInfo>, Self::Error>;

    /// Fetches the finalized head block info.
    async fn l2_finalized_head(&self) -> Result<Option<L2BlockInfo>, Self::Error>;
}
