//! Consensus execution commands and read errors.

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256};
use alloy_transport::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use base_common_chain_config::RollupConfig;
use base_common_client_ethereum::{EthGetBlock, Ethereum, Network};
use base_common_types_payload::{
    BaseExecutionPayloadEnvelope, BasePayloadAttributes, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus,
};
use base_consensus_batch::{FromBlockError, L2BlockInfo};
use thiserror::Error;

/// An error that occurred in the [`EngineClient`].
#[derive(Error, Debug)]
pub enum EngineClientError {
    /// An RPC error occurred
    #[error("An RPC error occurred: {0}")]
    RpcError(#[from] RpcError<TransportErrorKind>),

    /// A local execution command failed.
    #[error(transparent)]
    Execution(#[from] base_execution_engine_driver::ExecutionCommandError),
    /// Submitting a block to the execution driver failed.
    #[error(transparent)]
    Submit(#[from] base_common_types_payload::BeaconOnNewPayloadError),
    /// Native build attributes could not be decoded.
    #[error("invalid build attributes: {0}")]
    InvalidAttributes(String),
    /// Reading local execution state failed.
    #[error(transparent)]
    Local(#[from] base_consensus_source::LocalL2Error),

    /// An error occurred while decoding the payload
    #[error("An error occurred while decoding the payload: {0}")]
    BlockInfoDecodeError(#[from] FromBlockError),
}
impl EngineClientError {
    /// Whether the execution driver rejected the requested forkchoice state.
    pub fn is_invalid_forkchoice(&self) -> bool {
        match self {
            Self::Execution(base_execution_engine_driver::ExecutionCommandError::Forkchoice(
                base_common_types_payload::BeaconForkChoiceUpdateError::ForkchoiceUpdateError(
                    base_common_types_payload::ForkchoiceUpdateError::InvalidState,
                ),
            )) => true,
            _ => false,
        }
    }

    /// Whether a build request violates the active chain rules.
    pub fn is_invalid_attributes(&self) -> bool {
        matches!(
            self,
            Self::InvalidAttributes(_)
                | Self::Execution(
                    base_execution_engine_driver::ExecutionCommandError::InvalidAttributes(_)
                )
        )
    }
}

/// Commands and reads used by consensus, implemented by the local execution client.
#[async_trait]
pub trait EngineClient: Send + Sync {
    /// Submits a payload for execution.
    async fn submit_payload(
        &self,
        envelope: BaseExecutionPayloadEnvelope,
    ) -> Result<PayloadStatus, EngineClientError>;
    /// Applies forkchoice and optionally starts a build.
    async fn update_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<BasePayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineClientError>;
    /// Resolves the payload built for the supplied attributes.
    async fn resolve_payload(
        &self,
        id: PayloadId,
    ) -> Result<BaseExecutionPayloadEnvelope, EngineClientError>;

    /// Returns a reference to the inner [`RollupConfig`].
    fn cfg(&self) -> &RollupConfig;

    /// Fetches the L1 block with the provided `BlockId`.
    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse>;

    /// Fetches the L2 block with the provided `BlockId`.
    async fn get_l2_block(
        &self,
        block: BlockId,
    ) -> Result<Option<base_common_types_chain::SealedBlock>, EngineClientError>;

    /// Reads the account storage root at a specific L2 block.
    async fn storage_root(
        &self,
        address: Address,
        block: BlockId,
    ) -> Result<B256, EngineClientError>;

    /// Fetches the native L2 block for the given [`BlockNumberOrTag`].
    async fn l2_block_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<base_common_types_chain::SealedBlock>, EngineClientError>;

    /// Fetches the [`L2BlockInfo`] by [`BlockNumberOrTag`].
    async fn l2_block_info_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError>;

    /// Returns whether the execution layer reports an active sync.
    async fn el_syncing(&self) -> Result<bool, EngineClientError>;
}
