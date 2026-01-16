//! Engine processing request types.

use alloy_rpc_types_engine::PayloadId;
use kona_engine::SealTaskError;
use kona_node_service::{EngineActorRequest, EngineClientError, EngineRpcRequest};
use kona_protocol::{BlockInfo, OpAttributesWithParent};
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use tokio::sync::mpsc;

/// Engine processing request types.
///
/// These are internal requests that the processor handles after routing
/// from the main [`EngineActorRequest`] type.
#[derive(Debug)]
pub enum EngineProcessingRequest {
    /// Request to build a new block.
    Build {
        /// The attributes to build from.
        attributes: Box<OpAttributesWithParent>,
        /// Channel to send back the payload ID.
        result_tx: mpsc::Sender<PayloadId>,
    },
    /// Request to process derived L2 attributes (consolidation).
    ProcessDerivedL2Attributes(Box<OpAttributesWithParent>),
    /// Request to process a finalized L1 block.
    ProcessFinalizedL1Block(Box<BlockInfo>),
    /// Request to process an unsafe L2 block.
    ProcessUnsafeL2Block(Box<OpExecutionPayloadEnvelope>),
    /// Request to reset the engine forkchoice.
    Reset {
        /// Channel to send back the result.
        result_tx: mpsc::Sender<Result<(), EngineClientError>>,
    },
    /// Request to seal a block.
    Seal {
        /// The payload ID to seal.
        payload_id: PayloadId,
        /// The attributes for sealing.
        attributes: Box<OpAttributesWithParent>,
        /// Channel to send back the result.
        result_tx: mpsc::Sender<Result<OpExecutionPayloadEnvelope, SealTaskError>>,
    },
}

/// Result of routing an engine actor request.
#[derive(Debug)]
pub enum RoutedRequest {
    /// An RPC request.
    Rpc(EngineRpcRequest),
    /// A processing request.
    Processing(EngineProcessingRequest),
}

impl RoutedRequest {
    /// Routes an [`EngineActorRequest`] to the appropriate request type.
    pub fn from_actor_request(request: EngineActorRequest) -> Self {
        match request {
            EngineActorRequest::RpcRequest(rpc_req) => Self::Rpc(*rpc_req),
            EngineActorRequest::BuildRequest(req) => {
                Self::Processing(EngineProcessingRequest::Build {
                    attributes: Box::new(req.attributes),
                    result_tx: req.result_tx,
                })
            }
            EngineActorRequest::ProcessDerivedL2AttributesRequest(attrs) => {
                Self::Processing(EngineProcessingRequest::ProcessDerivedL2Attributes(attrs))
            }
            EngineActorRequest::ProcessFinalizedL1BlockRequest(block) => {
                Self::Processing(EngineProcessingRequest::ProcessFinalizedL1Block(block))
            }
            EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                Self::Processing(EngineProcessingRequest::ProcessUnsafeL2Block(envelope))
            }
            EngineActorRequest::ResetRequest(req) => {
                Self::Processing(EngineProcessingRequest::Reset { result_tx: req.result_tx })
            }
            EngineActorRequest::SealRequest(req) => {
                Self::Processing(EngineProcessingRequest::Seal {
                    payload_id: req.payload_id,
                    attributes: Box::new(req.attributes),
                    result_tx: req.result_tx,
                })
            }
        }
    }
}
