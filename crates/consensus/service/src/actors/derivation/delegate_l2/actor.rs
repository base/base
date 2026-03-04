use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_alloy_network::Base;
use base_consensus_engine::ConsolidateInput;
use base_protocol::L2BlockInfo;
use tokio::{select, sync::mpsc, time};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tracing::{debug, error, info, warn};

use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, EngineActorRequest,
    NodeActor,
    actors::derivation::{DerivationError, delegate_l2::L2SourceClient},
};

/// The [`NodeActor`] for the L2 delegate derivation sub-routine.
///
/// Polls a source L2 execution layer node for new blocks and drives the local
/// engine via `ProcessUnsafeL2BlockRequest` (`NewPayload` + FCU) rather than
/// running the full derivation pipeline.
///
/// Safe and finalized head updates are forwarded separately.
#[derive(Debug)]
pub struct DelegateL2DerivationActor<DerivationEngineClient_, L2Source = super::DelegateL2Client>
where
    DerivationEngineClient_: DerivationEngineClient,
    L2Source: L2SourceClient,
{
    cancellation_token: CancellationToken,
    inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
    engine_client: DerivationEngineClient_,
    engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
    local_l2_provider: RootProvider<Base>,
    l2_source: L2Source,
    local_head: u64,
}

impl<DerivationEngineClient_, L2Source> CancellableContext
    for DelegateL2DerivationActor<DerivationEngineClient_, L2Source>
where
    DerivationEngineClient_: DerivationEngineClient,
    L2Source: L2SourceClient,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

impl<DerivationEngineClient_, L2Source> DelegateL2DerivationActor<DerivationEngineClient_, L2Source>
where
    DerivationEngineClient_: DerivationEngineClient,
    L2Source: L2SourceClient,
{
    /// Creates a new [`DelegateL2DerivationActor`].
    pub fn new(
        engine_client: DerivationEngineClient_,
        engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
        cancellation_token: CancellationToken,
        inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
        local_l2_provider: RootProvider<Base>,
        l2_source: L2Source,
    ) -> Self {
        Self {
            cancellation_token,
            inbound_request_rx,
            engine_client,
            engine_actor_request_tx,
            local_l2_provider,
            l2_source,
            local_head: 0,
        }
    }
}

#[async_trait]
impl<DerivationEngineClient_, L2Source> NodeActor
    for DelegateL2DerivationActor<DerivationEngineClient_, L2Source>
where
    DerivationEngineClient_: DerivationEngineClient + 'static,
    L2Source: L2SourceClient + 'static,
{
    type Error = DerivationError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        self.run().await
    }
}

impl<DerivationEngineClient_, L2Source> DelegateL2DerivationActor<DerivationEngineClient_, L2Source>
where
    DerivationEngineClient_: DerivationEngineClient + 'static,
    L2Source: L2SourceClient + 'static,
{
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

    async fn run(mut self) -> Result<(), DerivationError> {
        if self.local_head == 0 {
            self.local_head = self
                .local_l2_provider
                .get_block_number()
                .await
                .map_err(|e| DerivationError::Sender(Box::new(e)))?;
        }

        info!(target: "derivation", local_head = self.local_head, "Starting L2 delegate derivation");
        let mut ticker = time::interval(Self::POLL_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            select! {
                biased;

                _ = self.cancellation_token.cancelled() => {
                    info!(target: "derivation", "Received shutdown signal. Exiting L2 delegate derivation.");
                    return Ok(());
                }
                req = self.inbound_request_rx.recv() => {
                    let Some(request_type) = req else {
                        error!(target: "derivation", "DelegateL2DerivationActor inbound request receiver closed unexpectedly");
                        self.cancellation_token.cancel();
                        return Err(DerivationError::RequestReceiveFailed);
                    };
                    self.handle_request(request_type).await?;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.sync_from_source().await {
                        warn!(target: "derivation", error = %e, "Failed to sync from L2 source");
                    }
                }
            }
        }
    }

    async fn handle_request(
        &mut self,
        request_type: DerivationActorRequest,
    ) -> Result<(), DerivationError> {
        match request_type {
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(safe_head) => {
                debug!(target: "derivation", safe_head = ?*safe_head, "Received safe head from engine.");
                self.local_head = safe_head.block_info.number;
            }
            DerivationActorRequest::ProcessEngineSyncCompletionRequest(safe_head) => {
                info!(target: "derivation", head = safe_head.block_info.number, "Engine sync completed.");
                self.local_head = safe_head.block_info.number;
            }
            DerivationActorRequest::ProcessEngineSignalRequest(_)
            | DerivationActorRequest::ProcessFinalizedL1Block(_)
            | DerivationActorRequest::ProcessL1HeadUpdateRequest(_) => {
                debug!(target: "derivation", request_type = ?request_type, "Ignoring request in L2 delegate mode");
            }
        }
        Ok(())
    }

    /// Polls the source L2 node for new blocks and inserts them into the local engine.
    async fn sync_from_source(&mut self) -> Result<(), DerivationError> {
        let remote_head = self
            .l2_source
            .get_block_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;

        if remote_head <= self.local_head {
            return Ok(());
        }

        for block_num in (self.local_head + 1)..=remote_head {
            if self.cancellation_token.is_cancelled() {
                info!(target: "derivation", block = block_num, "Sync interrupted by shutdown");
                return Ok(());
            }

            let payload = self
                .l2_source
                .get_payload_by_number(block_num)
                .await
                .map_err(|e| DerivationError::Sender(Box::new(e)))?;

            debug!(
                target: "derivation",
                block = block_num,
                "Inserting block from L2 source"
            );

            self.engine_actor_request_tx
                .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(
                    payload,
                )))
                .await
                .map_err(|_| {
                    DerivationError::Sender(Box::new(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "engine actor request channel closed",
                    )))
                })?;

            self.local_head = block_num;
        }

        self.update_safe_and_finalized().await?;

        Ok(())
    }

    async fn update_safe_and_finalized(&self) -> Result<(), DerivationError> {
        if let Ok(safe_number) = self.l2_source.get_block_number(BlockNumberOrTag::Safe).await {
            let clamped_safe = safe_number.min(self.local_head);
            if let Ok(safe_payload) =
                self.l2_source.get_payload_by_number(clamped_safe).await
            {
                let safe_l2 = L2BlockInfo {
                    block_info: base_protocol::BlockInfo {
                        hash: safe_payload.execution_payload.block_hash(),
                        number: clamped_safe,
                        ..Default::default()
                    },
                    ..Default::default()
                };

                let _ = self
                    .engine_client
                    .send_safe_l2_signal(ConsolidateInput::BlockInfo(safe_l2))
                    .await;
            }
        }

        if let Ok(finalized_number) = self
            .l2_source
            .get_block_number(BlockNumberOrTag::Finalized)
            .await
        {
            let clamped_finalized = finalized_number.min(self.local_head);
            let _ = self
                .engine_client
                .send_finalized_l2_block(clamped_finalized)
                .await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_alloy_rpc_types_engine::{OpExecutionPayload, OpExecutionPayloadEnvelope};
    use base_protocol::{BlockInfo, L2BlockInfo};
    use mockall::predicate::*;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::derivation::{
        delegate_l2::client::MockL2SourceClient,
        engine_client::MockDerivationEngineClient,
    };

    fn dummy_l2_block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number,
                hash: B256::from([number as u8; 32]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn dummy_payload_envelope(block_number: u64) -> OpExecutionPayloadEnvelope {
        let payload = ExecutionPayloadV1 {
            parent_hash: B256::ZERO,
            fee_recipient: alloy_primitives::Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: alloy_primitives::Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: alloy_primitives::Bytes::new(),
            base_fee_per_gas: alloy_primitives::U256::ZERO,
            block_hash: B256::from([block_number as u8; 32]),
            transactions: vec![],
        };
        OpExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: OpExecutionPayload::V1(payload),
        }
    }

    fn make_actor(
        engine_client: MockDerivationEngineClient,
        l2_source: MockL2SourceClient,
    ) -> (
        DelegateL2DerivationActor<MockDerivationEngineClient, MockL2SourceClient>,
        mpsc::Sender<DerivationActorRequest>,
        mpsc::Receiver<EngineActorRequest>,
        CancellationToken,
    ) {
        let cancel = CancellationToken::new();
        let (deriv_tx, deriv_rx) = mpsc::channel(16);
        let (engine_tx, engine_rx) = mpsc::channel(16);

        let local_l2_provider = RootProvider::<Base>::new_http(
            "http://localhost:1234".parse().unwrap(),
        );

        let actor = DelegateL2DerivationActor::new(
            engine_client,
            engine_tx,
            cancel.clone(),
            deriv_rx,
            local_l2_provider,
            l2_source,
        );

        (actor, deriv_tx, engine_rx, cancel)
    }

    #[tokio::test]
    async fn handle_sync_completion_enables_sync() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _, _, _) = make_actor(engine_client, l2_source);

        assert_eq!(actor.local_head, 0);

        let safe_head = dummy_l2_block_info(42);
        actor
            .handle_request(DerivationActorRequest::ProcessEngineSyncCompletionRequest(
                Box::new(safe_head),
            ))
            .await
            .unwrap();

        assert_eq!(actor.local_head, 42);
    }

    #[tokio::test]
    async fn handle_safe_head_update_sets_local_head() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _, _, _) = make_actor(engine_client, l2_source);

        let safe_head = dummy_l2_block_info(100);
        actor
            .handle_request(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(
                Box::new(safe_head),
            ))
            .await
            .unwrap();

        assert_eq!(actor.local_head, 100);
    }

    #[tokio::test]
    async fn handle_irrelevant_requests_noop() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _, _, _) = make_actor(engine_client, l2_source);

        actor
            .handle_request(DerivationActorRequest::ProcessL1HeadUpdateRequest(
                Box::new(BlockInfo::default()),
            ))
            .await
            .unwrap();

        actor
            .handle_request(DerivationActorRequest::ProcessFinalizedL1Block(
                Box::new(BlockInfo::default()),
            ))
            .await
            .unwrap();

        assert_eq!(actor.local_head, 0);
    }

    #[tokio::test]
    async fn sync_noop_when_remote_behind() {
        let engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(5));

        let (mut actor, _, _, _) = make_actor(engine_client, l2_source);
        actor.local_head = 10;

        actor.sync_from_source().await.unwrap();
        assert_eq!(actor.local_head, 10);
    }

    #[tokio::test]
    async fn sync_fetches_and_inserts_blocks() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(3));

        l2_source
            .expect_get_payload_by_number()
            .with(eq(1))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(2))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(3))
            .returning(|n| Ok(dummy_payload_envelope(n)));

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(2));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(2))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(1));

        engine_client
            .expect_send_safe_l2_signal()
            .returning(|_| Ok(()));
        engine_client
            .expect_send_finalized_l2_block()
            .returning(|_| Ok(()));

        let (mut actor, _, mut engine_rx, _) = make_actor(engine_client, l2_source);
        actor.local_head = 0;

        actor.sync_from_source().await.unwrap();
        assert_eq!(actor.local_head, 3);

        for expected_num in 1..=3 {
            let req = engine_rx.try_recv().unwrap();
            match req {
                EngineActorRequest::ProcessUnsafeL2BlockRequest(envelope) => {
                    assert_eq!(envelope.execution_payload.block_number(), expected_num);
                }
                other => panic!("Expected ProcessUnsafeL2BlockRequest, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn sync_aborts_on_cancellation() {
        let engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(100));

        let (mut actor, _, engine_rx, cancel) = make_actor(engine_client, l2_source);
        actor.local_head = 0;

        cancel.cancel();
        actor.sync_from_source().await.unwrap();

        assert_eq!(actor.local_head, 0);
        assert!(engine_rx.is_empty());
    }

    #[tokio::test]
    async fn run_loop_stops_on_cancellation() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _deriv_tx, _engine_rx, cancel) = make_actor(engine_client, l2_source);

        actor.local_head = 10;
        cancel.cancel();

        let result = actor.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_loop_errors_on_channel_close() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, deriv_tx, _engine_rx, _cancel) = make_actor(engine_client, l2_source);

        actor.local_head = 10;
        drop(deriv_tx);

        let result = actor.run().await;
        assert!(result.is_err());
    }
}
