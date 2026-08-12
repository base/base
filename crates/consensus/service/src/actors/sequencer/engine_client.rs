use std::{fmt::Debug, sync::Arc};

use alloy_rpc_types_engine::PayloadId;
use async_trait::async_trait;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_engine::EngineState;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use derive_more::Constructor;
use tokio::sync::{mpsc, watch};

use crate::{
    EngineClientError, EngineClientResult,
    actors::engine::{
        BuildRequest, EngineActorRequest, GetPayloadRequest, InsertUnsafePayloadRequest,
        ReconcileShadowRequest, ResetOrigin, ResetReason, ResetRequest,
    },
};

/// Trait to be used by the Sequencer to interact with the engine, abstracting communication
/// mechanism.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait SequencerEngineClient: Debug + Send + Sync {
    /// Resets the engine's forkchoice, awaiting confirmation that it succeeded or returning the
    /// error in performing the reset.
    async fn reset_engine_forkchoice(&self, reason: ResetReason) -> EngineClientResult<()>;

    /// Coordinates the engine boundary for a shadow cycle that the caller will explicitly rebuild.
    ///
    /// During initial catch-up, success activates shadow production after catch-up has already
    /// advanced the engine. Once shadow production is active, this performs an actual reset.
    async fn reset_engine_forkchoice_coordinated(
        &self,
        reason: ResetReason,
    ) -> EngineClientResult<()> {
        self.reset_engine_forkchoice(reason).await
    }

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

    /// Submits the sealed payload to the engine for insertion (`new_payload` + FCU), returning the
    /// inserted unsafe head after the engine acknowledges insertion.
    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<L2BlockInfo>;

    /// Returns the current unsafe head [`L2BlockInfo`].
    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo>;

    /// Replaces the shadow branch with the active sequencer's buffered P2P branch.
    async fn reconcile_shadow(
        &self,
        shadow_head: L2BlockInfo,
    ) -> EngineClientResult<Option<L2BlockInfo>> {
        let _ = shadow_head;
        Err(EngineClientError::ShadowReconciliationDisabled)
    }

    /// Returns whether the engine has completed execution-layer sync.
    async fn el_sync_finished(&self) -> EngineClientResult<bool>;
}

/// Blanket implementation so [`Arc<T>`] can be used wherever `T: SequencerEngineClient`.
///
/// Both [`crate::SequencerActor`] and [`super::build::PayloadBuilder`] hold an
/// `Arc` to the same engine client, so this impl allows both to call trait
/// methods without any additional wrapping.
#[async_trait]
impl<T: SequencerEngineClient> SequencerEngineClient for Arc<T> {
    async fn reset_engine_forkchoice(&self, reason: ResetReason) -> EngineClientResult<()> {
        (**self).reset_engine_forkchoice(reason).await
    }

    async fn reset_engine_forkchoice_coordinated(
        &self,
        reason: ResetReason,
    ) -> EngineClientResult<()> {
        (**self).reset_engine_forkchoice_coordinated(reason).await
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
    ) -> EngineClientResult<L2BlockInfo> {
        (**self).insert_unsafe_payload(payload).await
    }

    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo> {
        (**self).get_unsafe_head().await
    }

    async fn reconcile_shadow(
        &self,
        shadow_head: L2BlockInfo,
    ) -> EngineClientResult<Option<L2BlockInfo>> {
        (**self).reconcile_shadow(shadow_head).await
    }

    async fn el_sync_finished(&self) -> EngineClientResult<bool> {
        (**self).el_sync_finished().await
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
    /// A channel to receive the latest engine state.
    pub engine_state_rx: watch::Receiver<EngineState>,
}

impl QueuedSequencerEngineClient {
    async fn send_reset(&self, origin: ResetOrigin, reason: ResetReason) -> EngineClientResult<()> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        info!(target: "sequencer", "Sending reset request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ResetRequest(Box::new(ResetRequest {
                result_tx,
                origin,
                reason,
            })))
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
}

#[async_trait]
impl SequencerEngineClient for QueuedSequencerEngineClient {
    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo> {
        Ok(*self.unsafe_head_rx.borrow())
    }

    async fn el_sync_finished(&self) -> EngineClientResult<bool> {
        Ok(self.engine_state_rx.borrow().el_sync_finished)
    }

    async fn reconcile_shadow(
        &self,
        shadow_head: L2BlockInfo,
    ) -> EngineClientResult<Option<L2BlockInfo>> {
        let (result_tx, mut result_rx) = mpsc::channel(1);
        self.engine_actor_request_tx
            .send(EngineActorRequest::ReconcileShadowRequest(Box::new(ReconcileShadowRequest {
                shadow_head,
                result_tx,
            })))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;
        result_rx.recv().await.ok_or_else(|| {
            EngineClientError::ResponseError("response channel closed.".to_string())
        })?
    }

    async fn reset_engine_forkchoice(&self, reason: ResetReason) -> EngineClientResult<()> {
        self.send_reset(ResetOrigin::Sequencer, reason).await
    }

    async fn reset_engine_forkchoice_coordinated(
        &self,
        reason: ResetReason,
    ) -> EngineClientResult<()> {
        self.send_reset(ResetOrigin::ShadowCycleCoordinated, reason).await
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
                otel_cx: opentelemetry::Context::current(),
            })))
            .await
            .is_err()
        {
            return Err(EngineClientError::RequestError("request channel closed.".to_string()));
        }

        match payload_id_rx.recv().await {
            Some(Ok(payload_id)) => {
                trace!(target: "sequencer", ?payload_id, "Start build request successfully.");
                Ok(payload_id)
            }
            Some(Err(err)) => {
                info!(target: "sequencer", ?err, "Start build request failed.");
                Err(EngineClientError::StartBuildError(err))
            }
            None => {
                error!(target: "block_engine", "Failed to receive payload for initiated block build");
                Err(EngineClientError::ResponseError("response channel closed.".to_string()))
            }
        }
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
                otel_cx: opentelemetry::Context::current(),
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
    ) -> EngineClientResult<L2BlockInfo> {
        let (result_tx, mut result_rx) = mpsc::channel(1);

        trace!(target: "sequencer", "Sending insert unsafe payload request to engine.");
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(Box::new(
                InsertUnsafePayloadRequest {
                    envelope: payload,
                    result_tx: Some(result_tx),
                    otel_cx: opentelemetry::Context::current(),
                },
            )))
            .await
            .map_err(|_| EngineClientError::RequestError("request channel closed.".to_string()))?;

        let inserted_head = match result_rx.recv().await {
            Some(Ok(inserted_head)) => inserted_head,
            Some(Err(err)) => {
                info!(target: "sequencer", error = ?err, "Insert unsafe payload failed");
                return Err(EngineClientError::InsertError(err));
            }
            None => {
                error!(target: "block_engine", "Failed to receive insert unsafe payload result");
                return Err(EngineClientError::ResponseError(
                    "response channel closed.".to_string(),
                ));
            }
        };

        trace!(
            target: "sequencer",
            block_number = inserted_head.block_info.number,
            block_hash = %inserted_head.block_info.hash,
            "Insert unsafe payload acknowledged"
        );

        Ok(inserted_head)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::EngineState;
    use base_protocol::{BlockInfo, L2BlockInfo};
    use tokio::sync::{mpsc, watch};

    use super::{QueuedSequencerEngineClient, SequencerEngineClient};
    use crate::EngineActorRequest;

    fn dummy_envelope() -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash: B256::with_last_byte(1),
                transactions: vec![],
            }),
        }
    }

    fn l2_head(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo::new(B256::with_last_byte(number as u8), number, B256::ZERO, 1),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn el_sync_finished_tracks_engine_state() {
        let (request_tx, _request_rx) = mpsc::channel(1);
        let (_, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let (engine_state_tx, engine_state_rx) = watch::channel(EngineState::default());
        let client = QueuedSequencerEngineClient::new(request_tx, unsafe_head_rx, engine_state_rx);

        assert!(!client.el_sync_finished().await.expect("read engine state"));

        engine_state_tx.send_replace(EngineState { el_sync_finished: true, ..Default::default() });

        assert!(client.el_sync_finished().await.expect("read updated engine state"));
    }

    #[tokio::test]
    async fn insert_unsafe_payload_returns_engine_ack() {
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (_, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let (_, engine_state_rx) = watch::channel(EngineState::default());
        let inserted_head = l2_head(1);
        let client = QueuedSequencerEngineClient::new(request_tx, unsafe_head_rx, engine_state_rx);

        let insert_handle =
            tokio::spawn(async move { client.insert_unsafe_payload(dummy_envelope()).await });

        let request = request_rx.recv().await.expect("insert request");
        let EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(request) = request else {
            panic!("expected local unsafe insert request");
        };
        let result_tx = request.result_tx.expect("insert result sender");
        result_tx.send(Ok(inserted_head)).await.expect("send insert result");

        let result = insert_handle.await.expect("insert task");

        assert_eq!(result.expect("insert result"), inserted_head);
    }

    #[tokio::test]
    async fn reconcile_shadow_returns_engine_ack() {
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (_, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let (_, engine_state_rx) = watch::channel(EngineState::default());
        let shadow_head = l2_head(12);
        let reconciled_head = l2_head(12);
        let client = QueuedSequencerEngineClient::new(request_tx, unsafe_head_rx, engine_state_rx);

        let reconcile_handle =
            tokio::spawn(async move { client.reconcile_shadow(shadow_head).await });

        let request = request_rx.recv().await.expect("reconciliation request");
        let EngineActorRequest::ReconcileShadowRequest(request) = request else {
            panic!("expected shadow reconciliation request");
        };
        assert_eq!(request.shadow_head, shadow_head);
        request
            .result_tx
            .send(Ok(Some(reconciled_head)))
            .await
            .expect("send reconciliation result");

        let result = reconcile_handle.await.expect("reconciliation task");

        assert_eq!(result.expect("reconciliation result"), Some(reconciled_head));
    }
}
