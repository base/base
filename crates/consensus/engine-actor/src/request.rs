//! Request types for the engine actor.

use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadId;
use kona_protocol::L2BlockInfo;
use op_alloy_rpc_types_engine::{OpExecutionPayloadEnvelopeV3, OpPayloadAttributes};
use tokio::sync::oneshot;

use crate::ProcessorError;

/// Top-level request type for the engine actor.
#[derive(Debug)]
pub enum EngineActorRequest {
    /// Build a new payload.
    Build(BuildRequest),
    /// Seal the current payload.
    Seal(SealRequest),
    /// Process a safe L2 signal (consolidation).
    ProcessSafeL2Signal(ProcessSafeL2SignalRequest),
    /// Process a finalized L2 block number.
    ProcessFinalizedL2BlockNumber(ProcessFinalizedL2BlockNumberRequest),
    /// Process an unsafe L2 block.
    ProcessUnsafeL2Block(Box<ProcessUnsafeL2BlockRequest>),
    /// Reset to the finalized head.
    Reset(ResetRequest),
}

/// Request to build a new payload.
#[derive(Debug)]
pub struct BuildRequest {
    /// The parent block hash to build on.
    pub parent_hash: B256,
    /// The payload attributes for the new block.
    pub attributes: OpPayloadAttributes,
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<PayloadId, ProcessorError>>,
}

impl BuildRequest {
    /// Creates a new build request.
    pub fn new(
        parent_hash: B256,
        attributes: OpPayloadAttributes,
    ) -> (Self, oneshot::Receiver<Result<PayloadId, ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { parent_hash, attributes, response: tx }, rx)
    }
}

/// Request to seal the current payload.
#[derive(Debug)]
pub struct SealRequest {
    /// The payload ID to seal.
    pub payload_id: PayloadId,
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<OpExecutionPayloadEnvelopeV3, ProcessorError>>,
}

impl SealRequest {
    /// Creates a new seal request.
    pub fn new(
        payload_id: PayloadId,
    ) -> (Self, oneshot::Receiver<Result<OpExecutionPayloadEnvelopeV3, ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { payload_id, response: tx }, rx)
    }
}

/// Request to process a safe L2 signal (consolidation).
#[derive(Debug)]
pub struct ProcessSafeL2SignalRequest {
    /// The safe L2 block info.
    pub safe_head: L2BlockInfo,
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<(), ProcessorError>>,
}

impl ProcessSafeL2SignalRequest {
    /// Creates a new safe L2 signal request.
    pub fn new(safe_head: L2BlockInfo) -> (Self, oneshot::Receiver<Result<(), ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { safe_head, response: tx }, rx)
    }
}

/// Request to process a finalized L2 block number.
#[derive(Debug)]
pub struct ProcessFinalizedL2BlockNumberRequest {
    /// The finalized L2 block number.
    pub finalized_number: u64,
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<(), ProcessorError>>,
}

impl ProcessFinalizedL2BlockNumberRequest {
    /// Creates a new finalized L2 block number request.
    pub fn new(finalized_number: u64) -> (Self, oneshot::Receiver<Result<(), ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { finalized_number, response: tx }, rx)
    }
}

/// Request to process an unsafe L2 block.
#[derive(Debug)]
pub struct ProcessUnsafeL2BlockRequest {
    /// The unsafe block envelope.
    pub envelope: OpExecutionPayloadEnvelopeV3,
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<(), ProcessorError>>,
}

impl ProcessUnsafeL2BlockRequest {
    /// Creates a new unsafe block request.
    pub fn new(
        envelope: OpExecutionPayloadEnvelopeV3,
    ) -> (Self, oneshot::Receiver<Result<(), ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { envelope, response: tx }, rx)
    }
}

/// Request to reset to the finalized head.
#[derive(Debug)]
pub struct ResetRequest {
    /// Channel to send the response.
    pub response: oneshot::Sender<Result<L2BlockInfo, ProcessorError>>,
}

impl ResetRequest {
    /// Creates a new reset request.
    pub fn new() -> (Self, oneshot::Receiver<Result<L2BlockInfo, ProcessorError>>) {
        let (tx, rx) = oneshot::channel();
        (Self { response: tx }, rx)
    }
}

impl Default for ResetRequest {
    fn default() -> Self {
        Self::new().0
    }
}
