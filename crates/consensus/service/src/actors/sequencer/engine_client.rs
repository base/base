use std::{fmt::Debug, sync::Arc};

use alloy_rpc_types_engine::PayloadId;
use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use derive_more::Constructor;
use tokio::sync::{mpsc, watch};

use crate::{
    EngineClientError, EngineClientResult,
    actors::engine::{BuildRequest, EngineActorRequest, GetPayloadRequest, ResetRequest},
};

/// Trait to be used by the Sequencer to interact with the engine, abstracting communication
/// mechanism.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait SequencerEngineClient: Debug + Send + Sync {
    /// Resets the engine's forkchoice, awaiting confirmation that it succeeded or returning the
    /// error in performing the reset.
    async fn reset_engine_forkchoice(&self) -> EngineClientResult<()>;

    /// Starts building a block with the provided attributes.
    ///
    /// Returns a `PayloadId` that can be used to seal the block later.
    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<PayloadId>;

    /// Fetches the sealed payload envelope from the engine WITHOUT inserting it.
    /// Call this before attempting conductor commit, then call `insert_unsafe_payload` on success.
    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<BaseExecutionPayloadEnvelope>;

    /// Fire-and-forget: submits the sealed payload to the engine for insertion (`new_payload` + FCU).
    /// Call this after a successful conductor commit.
    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()>;

    /// Returns the current unsafe head [`L2BlockInfo`].
    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo>;
}

/// Blanket implementation so [`Arc<T>`] can be used wherever `T: SequencerEngineClient`.
///
/// Both [`crate::SequencerActor`] and [`super::build::PayloadBuilder`] hold an
/// `Arc` to the same engine client, so this impl allows both to call trait
/// methods without any additional wrapping.
#[async_trait]
impl<T: SequencerEngineClient> SequencerEngineClient for Arc<T> {
    async fn reset_engine_forkchoice(&self) -> EngineClientResult<()> {
        (**self).reset_engine_forkchoice().await
    }

    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<PayloadId> {
        (**self).start_build_block(attributes).await
    }

    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<BaseExecutionPayloadEnvelope> {
        (**self).get_sealed_payload(payload_id, attributes).await
    }

    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()> {
        (**self).insert_unsafe_payload(payload).await
    }

    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo> {
        (**self).get_unsafe_head().await
    }
}

/// Queue-based implementation of the [`SequencerEngineClient`] trait. This handles all
/// channel-based communication.
#[derive(Constructor, Debug)]
pub struct QueuedSequencerEngineClient {
    /// A channel to use to send the `EngineActor` requests.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
    /// A channel to receive the latest unsafe head [`L2BlockInfo`].
    pub unsafe_head_rx: watch::Receiver<L2BlockInfo>,
}

#[async_trait]
impl SequencerEngineClient for QueuedSequencerEngineClient {
    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo> {
        Ok(*self.unsafe_head_rx.borrow())
    }

    async fn reset_engine_forkchoice(&self) -> EngineClientResult<()> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        info!(target: "sequencer", "Sending reset request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest { result_tx })))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        result_rx
            .recv()
            .await
            .inspect(|_| info!(target: "sequencer", "Engine reset successfully."))
            .ok_or_else(|| {
                error!(target: "block_engine", "Failed to receive forkchoice reset result");
                EngineClientError::ResponseError("response channel closed.".to_string())
            })?
    }

    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<PayloadId> {
        let (payload_id_tx, mut payload_id_rx) = mpsc::channel(1);

        trace!(target: "sequencer", "Sending start build request to engine.");
        if self
            .engine_actor_request_tx
            .send(EngineActorRequest::BuildRequest(Box::new(BuildRequest {
                attributes,
                result_tx: payload_id_tx,
            })))
            .await
            .is_err()
        {
            return Err(EngineClientError::RequestError("request channel closed.".to_string()));
        }

        payload_id_rx.recv()
            .await
            .inspect(|payload_id| trace!(target: "sequencer", ?payload_id, "Start build request successfully."))
            .ok_or_else(|| {
            error!(target: "block_engine", "Failed to receive payload for initiated block build");
            EngineClientError::ResponseError("response channel closed.".to_string())
        })
    }

    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<BaseExecutionPayloadEnvelope> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        trace!(target: "sequencer", ?attributes, "Sending get payload request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::GetPayloadRequest(Box::new(GetPayloadRequest {
                payload_id,
                attributes,
                result_tx,
            })))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        match result_rx.recv().await {
            Some(Ok(payload)) => {
                trace!(target: "sequencer", ?payload, "Get payload succeeded.");
                Ok(payload)
            }
            Some(Err(err)) => {
                info!(target: "sequencer", ?err, "Get payload failed.");
                Err(EngineClientError::SealError(err))
            }
            None => {
                error!(target: "block_engine", "Failed to receive built payload");
                Err(EngineClientError::ResponseError("response channel closed.".to_string()))
            }
        }
    }

    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<()> {
        trace!(target: "sequencer", "Sending insert unsafe payload request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(payload)))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))
    }
}
