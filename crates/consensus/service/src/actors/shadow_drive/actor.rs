//! The [`ShadowDriveActor`].

use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BaseExecutionPayloadEnvelope;
use base_consensus_derive::{AttributesBuilder, PipelineErrorKind};
use base_consensus_engine::ReanchorTaskError;
use base_protocol::{AttributesWithParent, L2BlockInfo};
use opentelemetry::Context as OtelContext;
use tokio::{
    select,
    sync::{mpsc, watch},
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, EngineActorRequest, EngineClientError, NodeActor, ShadowDriveConfig,
    ShadowReanchorRequest,
    actors::{OriginSelector, SequencerEngineClient},
    follow::RemoteClient,
};

const SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The [`ShadowDriveActor`] coordinates shadow-drive candidate builds and re-anchors.
///
/// ## Forkchoice authority
///
/// This actor **never** issues Engine API forkchoice updates directly. Re-anchors are routed
/// through the node's single `EngineActor` via `EngineActorRequest::ProcessShadowReanchorRequest`
/// so there is only one forkchoice authority driving the execution layer.
#[derive(Debug)]
pub struct ShadowDriveActor<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_>
where
    AttributesBuilder_: AttributesBuilder + Sync,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    Source_: RemoteClient,
{
    /// The payload attributes builder.
    pub attributes_builder: AttributesBuilder_,
    /// The L1 origin selector.
    pub origin_selector: OriginSelector_,
    /// The engine client used for build + commit.
    pub engine_client: Arc<SequencerEngineClient_>,
    /// Channel for issuing re-anchor requests.
    pub engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
    /// Canonical source client.
    pub source: Arc<Source_>,
    /// Rollup configuration.
    pub rollup_config: Arc<RollupConfig>,
    /// Shadow-drive runtime configuration.
    pub shadow_config: ShadowDriveConfig,
    /// Cancellation token.
    pub cancellation_token: CancellationToken,
    /// Engine state watch channel for coherence checks.
    pub engine_state_rx: watch::Receiver<base_consensus_engine::EngineState>,
}

#[derive(Debug, thiserror::Error)]
/// Errors produced by the [`ShadowDriveActor`] during its per-slot build/commit/re-anchor loop.
pub enum ShadowDriveActorError {
    /// Shadow-drive configuration is invalid.
    #[error("shadow-drive max reorg depth must be > 0")]
    InvalidMaxReorgDepth,
    /// Failed to fetch from the canonical source.
    #[error(transparent)]
    Source(#[from] crate::RemoteL2ClientError),
    /// Engine client error during build or commit.
    #[error(transparent)]
    Engine(#[from] EngineClientError),
    /// Origin selector error.
    #[error(transparent)]
    Origin(#[from] crate::L1OriginSelectorError),
    /// Payload attribute preparation failed.
    #[error(transparent)]
    Attributes(#[from] PipelineErrorKind),
    /// Failed to decode L2 block info from a payload.
    #[error(transparent)]
    BlockInfo(#[from] base_protocol::FromBlockError),
    /// Failed to send a re-anchor request to the engine actor.
    #[error("failed to send re-anchor request to engine actor")]
    ReanchorRequestSend,
    /// Re-anchor response channel closed unexpectedly.
    #[error("re-anchor response channel closed")]
    ReanchorResponseClosed,
    /// The build deadline elapsed before the source produced the next head.
    #[error("shadow-drive build deadline exceeded waiting for block {expected} (latest={latest})")]
    BuildDeadlineExceeded {
        /// The block number the actor was waiting for.
        expected: u64,
        /// The latest block number observed from the source at time of failure.
        latest: u64,
    },
    /// Re-anchor task failed.
    #[error(transparent)]
    Reanchor(#[from] ReanchorTaskError),
}

impl<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_>
    ShadowDriveActor<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_>
where
    AttributesBuilder_: AttributesBuilder + Send + Sync,
    OriginSelector_: OriginSelector + Send,
    SequencerEngineClient_: SequencerEngineClient + Send + Sync,
    Source_: RemoteClient + Send + Sync,
{
    async fn fetch_source_payload(&self, number: u64) -> Result<BaseExecutionPayloadEnvelope, ShadowDriveActorError> {
        Ok(self.source.get_payload_by_number(number).await?)
    }

    async fn fetch_source_head(&self) -> Result<L2BlockInfo, ShadowDriveActorError> {
        let latest = self.source.get_block_number(BlockNumberOrTag::Latest).await?;
        let payload = self.fetch_source_payload(latest).await?;
        let info = L2BlockInfo::from_payload_and_genesis(
            payload.execution_payload.clone(),
            payload.parent_beacon_block_root,
            &self.rollup_config.genesis,
        )?;
        Ok(info)
    }

    async fn build_candidate(
        &mut self,
        parent: L2BlockInfo,
    ) -> Result<L2BlockInfo, ShadowDriveActorError> {
        let l1_origin = self.origin_selector.next_l1_origin(parent, false).await?;
        let attributes = self
            .attributes_builder
            .prepare_payload_attributes(parent, l1_origin.id())
            .await?;
        let attrs_with_parent = AttributesWithParent::new(attributes, parent, None, false);

        let payload_id = self.engine_client.start_build_block(attrs_with_parent.clone()).await?;
        let payload = self.engine_client.get_sealed_payload(payload_id, attrs_with_parent).await?;
        let inserted_head = self.engine_client.insert_unsafe_payload(payload).await?;

        Ok(inserted_head)
    }

    async fn wait_for_real_next_payload(
        &self,
        expected_number: u64,
    ) -> Result<(BaseExecutionPayloadEnvelope, L2BlockInfo), ShadowDriveActorError> {
        let deadline = Instant::now() + self.shadow_config.build_deadline;

        loop {
            let latest = self.source.get_block_number(BlockNumberOrTag::Latest).await?;
            if latest >= expected_number {
                let payload = self.fetch_source_payload(expected_number).await?;
                let info = L2BlockInfo::from_payload_and_genesis(
                    payload.execution_payload.clone(),
                    payload.parent_beacon_block_root,
                    &self.rollup_config.genesis,
                )?;
                return Ok((payload, info));
            }

            if Instant::now() >= deadline {
                return Err(ShadowDriveActorError::BuildDeadlineExceeded {
                    expected: expected_number,
                    latest,
                });
            }

            sleep(SOURCE_POLL_INTERVAL).await;
        }
    }

    async fn reanchor_to(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> Result<L2BlockInfo, ShadowDriveActorError> {
        let (result_tx, mut result_rx) = mpsc::channel(1);
        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessShadowReanchorRequest(Box::new(
                ShadowReanchorRequest { envelope: payload, result_tx, otel_cx: OtelContext::current() },
            )))
            .await
            .map_err(|_| ShadowDriveActorError::ReanchorRequestSend)?;

        result_rx
            .recv()
            .await
            .ok_or(ShadowDriveActorError::ReanchorResponseClosed)?
            .map_err(ShadowDriveActorError::Reanchor)
    }

    async fn assert_engine_head(&self, expected: L2BlockInfo) {
        let mut state_rx = self.engine_state_rx.clone();
        let expected_hash = expected.block_info.hash;
        let expected_number = expected.block_info.number;
        let wait_result = timeout(Duration::from_secs(2), state_rx.wait_for(|state| {
            let head = state.sync_state.unsafe_head();
            head.block_info.hash == expected_hash && head.block_info.number == expected_number
        }))
        .await;
        let success = matches!(wait_result, Ok(Ok(_)));
        drop(wait_result);

        if success {
            debug_assert!(state_rx.borrow().sync_state.unsafe_head() == expected);
        } else {
            let current = state_rx.borrow().sync_state.unsafe_head();
            error!(
                target: "shadow_drive",
                expected_hash = %expected_hash,
                expected_number,
                actual_hash = %current.block_info.hash,
                actual_number = current.block_info.number,
                "ShadowDrive re-anchor did not update unsafe head"
            );
        }
    }

    async fn run_slot(&mut self, parent: L2BlockInfo) -> Result<L2BlockInfo, ShadowDriveActorError> {
        let reorg_depth = 1u64;
        if reorg_depth > self.shadow_config.max_reorg_depth {
            warn!(
                target: "shadow_drive",
                reorg_depth,
                max_reorg_depth = self.shadow_config.max_reorg_depth,
                "ShadowDrive reorg depth exceeds configured maximum; skipping slot"
            );
            return self.fetch_source_head().await;
        }

        let candidate_head = self.build_candidate(parent).await?;
        info!(
            target: "shadow_drive",
            candidate_number = candidate_head.block_info.number,
            candidate_hash = %candidate_head.block_info.hash,
            reorg_depth,
            "ShadowDrive candidate committed"
        );

        let (real_payload, real_head) = match self
            .wait_for_real_next_payload(parent.block_info.number.saturating_add(1))
            .await
        {
            Ok(result) => result,
            Err(ShadowDriveActorError::BuildDeadlineExceeded { expected, latest }) => {
                warn!(
                    target: "shadow_drive",
                    expected,
                    latest,
                    deadline = ?self.shadow_config.build_deadline,
                    "ShadowDrive build deadline exceeded; re-anchoring to parent"
                );
                let parent_payload = self.fetch_source_payload(parent.block_info.number).await?;
                let reanchored = self.reanchor_to(parent_payload).await?;
                return Ok(reanchored);
            }
            Err(err) => return Err(err),
        };

        let reanchored = self.reanchor_to(real_payload).await?;
        info!(
            target: "shadow_drive",
            reanchor_number = reanchored.block_info.number,
            reanchor_hash = %reanchored.block_info.hash,
            reorg_depth,
            "ShadowDrive re-anchor applied"
        );

        self.assert_engine_head(real_head).await;

        Ok(real_head)
    }
}

#[async_trait]
impl<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_> NodeActor
    for ShadowDriveActor<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_>
where
    AttributesBuilder_: AttributesBuilder + Send + Sync + 'static,
    OriginSelector_: OriginSelector + Send + 'static,
    SequencerEngineClient_: SequencerEngineClient + Send + Sync + 'static,
    Source_: RemoteClient + Send + Sync + 'static,
{
    type Error = ShadowDriveActorError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        if self.shadow_config.max_reorg_depth == 0 {
            return Err(ShadowDriveActorError::InvalidMaxReorgDepth);
        }

        info!(target: "shadow_drive", "ShadowDrive active");
        let mut parent = self.fetch_source_head().await?;
        let cancellation = self.cancellation_token.clone();

        loop {
            select! {
                _ = cancellation.cancelled() => {
                    info!(target: "shadow_drive", "Received shutdown signal. Exiting ShadowDrive task.");
                    return Ok(());
                }
                result = self.run_slot(parent) => {
                    parent = result?;
                }
            }
        }
    }
}

impl<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_> CancellableContext
    for ShadowDriveActor<AttributesBuilder_, OriginSelector_, SequencerEngineClient_, Source_>
where
    AttributesBuilder_: AttributesBuilder + Sync,
    OriginSelector_: OriginSelector,
    SequencerEngineClient_: SequencerEngineClient,
    Source_: RemoteClient,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bloom, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope, BasePayloadAttributes};
    use base_protocol::{BlockInfo, L2BlockInfo, L1BlockInfoBedrock};
    use base_consensus_derive::test_utils::TestAttributesBuilder;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::ShadowDriveActor;
    use crate::{
        EngineActorRequest, MockOriginSelector, MockRemoteClient, MockSequencerEngineClient,
        ShadowDriveConfig,
    };

    fn l1_info_deposit_tx() -> Vec<u8> {
        BaseTxEnvelope::from(TxDeposit {
            input: L1BlockInfoBedrock::default().encode_calldata(),
            ..Default::default()
        })
        .encoded_2718()
    }

    fn payload_envelope(block_number: u64, parent_hash: B256, block_hash: B256) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: block_number,
                extra_data: Default::default(),
                base_fee_per_gas: U256::ZERO,
                block_hash,
                transactions: vec![l1_info_deposit_tx().into()],
            }),
        }
    }

    fn block_info(number: u64, hash: B256, parent_hash: B256) -> L2BlockInfo {
        L2BlockInfo { block_info: BlockInfo::new(hash, number, parent_hash, number), ..Default::default() }
    }

    #[tokio::test]
    async fn shadow_drive_commit_and_reanchor_once() {
        let rollup_config = Arc::new(RollupConfig::default());
        let parent_hash = B256::with_last_byte(1);
        let candidate_hash = B256::with_last_byte(2);
        let real_hash = B256::with_last_byte(3);
        let parent = block_info(1, parent_hash, B256::ZERO);

        let candidate_payload = payload_envelope(2, parent_hash, candidate_hash);
        let real_payload = payload_envelope(2, parent_hash, real_hash);
        let reanchor_head = L2BlockInfo::from_payload_and_genesis(
            real_payload.execution_payload.clone(),
            real_payload.parent_beacon_block_root,
            &rollup_config.genesis,
        )
        .expect("real head");

        let mut origin_selector = MockOriginSelector::new();
        origin_selector
            .expect_next_l1_origin()
            .returning(|_, _| Ok(BlockInfo::new(B256::with_last_byte(9), 10, B256::ZERO, 0)));

        let mut engine_client = MockSequencerEngineClient::new();
        engine_client.expect_start_build_block().returning(|_| Ok(alloy_rpc_types_engine::PayloadId::new([1; 8])));
        engine_client
            .expect_get_sealed_payload()
            .returning(move |_, _| Ok(candidate_payload.clone()));
        engine_client
            .expect_insert_unsafe_payload()
            .returning(move |_| Ok(block_info(2, candidate_hash, parent_hash)));

        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().returning(|_| Ok(2));
        source.expect_get_payload_by_number().returning(move |_| Ok(real_payload.clone()));

        let (engine_actor_request_tx, mut engine_actor_request_rx) = mpsc::channel(1);
        let (state_tx, state_rx) = watch::channel(base_consensus_engine::EngineState::default());

        let mut actor = ShadowDriveActor {
            attributes_builder: TestAttributesBuilder {
                attributes: vec![Ok(BasePayloadAttributes::default())],
            },
            origin_selector,
            engine_client: Arc::new(engine_client),
            engine_actor_request_tx,
            source: Arc::new(source),
            rollup_config: Arc::clone(&rollup_config),
            shadow_config: ShadowDriveConfig {
                source_l2_rpc: "http://localhost:9545".parse().expect("url"),
                build_deadline: Duration::from_secs(1),
                max_reorg_depth: 1,
            },
            cancellation_token: CancellationToken::new(),
            engine_state_rx: state_rx,
        };

        let reanchor_task = tokio::spawn(async move {
            let request = engine_actor_request_rx.recv().await.expect("reanchor request");
            let EngineActorRequest::ProcessShadowReanchorRequest(request) = request else {
                panic!("expected reanchor request");
            };
            request
                .result_tx
                .send(Ok(reanchor_head))
                .await
                .expect("send reanchor result");

            let mut state = base_consensus_engine::EngineState::default();
            state.sync_state = state.sync_state.apply_update(base_consensus_engine::EngineSyncStateUpdate {
                unsafe_head: Some(reanchor_head),
                ..Default::default()
            });
            state_tx.send_replace(state);
        });

        let next = actor.run_slot(parent).await.expect("shadow drive slot");
        assert_eq!(next.block_info.hash, reanchor_head.block_info.hash);
        reanchor_task.await.expect("reanchor task");
    }
}
