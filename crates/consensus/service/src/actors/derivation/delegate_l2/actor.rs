use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_alloy_network::Base;
use base_consensus_engine::DelegatedForkchoiceUpdate;
use base_protocol::L2BlockInfo;
use futures::future::OptionFuture;
use serde::Deserialize;
use tokio::{select, sync::mpsc, task::JoinHandle, time};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tracing::{debug, error, info, warn};

use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, EngineActorRequest,
    NodeActor,
    actors::derivation::{DerivationError, delegate_l2::L2SourceClient},
};

const DEFAULT_PROOFS_MAX_BLOCKS_AHEAD: u64 = 512;

#[derive(Debug, Deserialize)]
struct ProofsSyncStatus {
    latest: Option<u64>,
}

/// The [`NodeActor`] for the L2 delegate derivation sub-routine.
///
/// Polls a source L2 execution layer node for new blocks and drives the local
/// engine via `ProcessUnsafeL2BlockRequest` (`NewPayload` + FCU) rather than
/// running the full derivation pipeline.
///
/// Safe and finalized head updates are forwarded together as delegated labels.
#[derive(Debug)]
pub struct DelegateL2DerivationActor<DerivationEngineClient_, L2Source = super::DelegateL2Client>
where
    DerivationEngineClient_: DerivationEngineClient,
    L2Source: L2SourceClient,
{
    cancellation_token: CancellationToken,
    inbound_request_rx: mpsc::Receiver<DerivationActorRequest>,
    engine_client: Arc<DerivationEngineClient_>,
    engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
    local_l2_provider: RootProvider<Base>,
    l2_source: Arc<L2Source>,
    sent_head: u64,
    proofs_enabled: bool,
    proofs_max_blocks_ahead: u64,
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
            engine_client: Arc::new(engine_client),
            engine_actor_request_tx,
            local_l2_provider,
            l2_source: Arc::new(l2_source),
            sent_head: 0,
            proofs_enabled: false,
            proofs_max_blocks_ahead: DEFAULT_PROOFS_MAX_BLOCKS_AHEAD,
        }
    }

    /// Enables proofs sync gating. When enabled, sync will not advance beyond
    /// `proofs_latest + proofs_max_blocks_ahead` to prevent proofs from
    /// falling too far behind.
    pub const fn with_proofs(mut self, enabled: bool) -> Self {
        self.proofs_enabled = enabled;
        self
    }

    /// Sets the maximum number of blocks the node may advance beyond the
    /// proofs `ExEx` head.
    pub const fn with_proofs_max_blocks_ahead(mut self, max_blocks_ahead: u64) -> Self {
        self.proofs_max_blocks_ahead = max_blocks_ahead;
        self
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
        if self.sent_head == 0 {
            let head = self
                .local_l2_provider
                .get_block_number()
                .await
                .map_err(|e| DerivationError::Sender(Box::new(e)))?;
            self.sent_head = head;
        }

        info!(target: "derivation", head = self.sent_head, "Starting L2 delegate derivation");
        let mut ticker = time::interval(Self::POLL_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut sync_task: Option<JoinHandle<Result<u64, DerivationError>>> = None;

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
                // Poll the sync task for completion without blocking.
                // `OptionFuture<&mut JoinHandle>` resolves immediately to
                // `None` when no task is in flight, letting us fall through
                // to spawn a new one.
                Some(result) = OptionFuture::from(sync_task.as_mut()) => {
                    sync_task = None;
                    match result {
                        Err(join_error) => {
                            error!(target: "derivation", error = %join_error, "Sync task panicked or was cancelled");
                        }
                        Ok(Err(derivation_error)) => {
                            warn!(target: "derivation", error = %derivation_error, "Sync from source failed");
                        }
                        Ok(Ok(new_sent_head)) => {
                            self.sent_head = new_sent_head;
                        }
                    }
                }
                _ = ticker.tick() => {
                    if sync_task.is_some() {
                        debug!(target: "derivation", "Sync already in progress, skipping tick");
                        continue;
                    }

                    let target_block = match self.determine_target_block().await {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            warn!(target: "derivation", sent_head = self.sent_head, "Target is behind already sent head, skipping sync");
                            continue;
                        },
                        Err(e) => {
                            warn!(target: "derivation", error = %e, "Failed to determine target block");
                            continue;
                        }
                    };
                    info!(target: "derivation", target_block, sent_head = self.sent_head, "Starting sync from L2 source");

                    let cancellation_token = self.cancellation_token.clone();
                    let l2_source = Arc::clone(&self.l2_source);
                    let engine_client = Arc::clone(&self.engine_client);
                    let engine_actor_request_tx = self.engine_actor_request_tx.clone();
                    let local_l2_provider = self.local_l2_provider.clone();
                    let sent_head = self.sent_head;

                    sync_task = Some(tokio::spawn(async move {
                        SyncFromSourceTask::new(
                            engine_client,
                            engine_actor_request_tx,
                            cancellation_token,
                            local_l2_provider,
                            sent_head,
                            target_block,
                            l2_source,
                        )
                        .sync_from_source()
                        .await
                    }));
                }
            }
        }
    }

    async fn determine_target_block(&self) -> Result<Option<u64>, DerivationError> {
        let remote_head = self
            .l2_source
            .get_block_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;

        let sync_limit = if self.proofs_enabled {
            match self
                .local_l2_provider
                .raw_request::<_, ProofsSyncStatus>("debug_proofsSyncStatus".into(), ())
                .await
            {
                Ok(status) => {
                    // default to 0 if proofs not available since user intends to avoid syncing past proofs head which is unknown
                    let latest = status.latest.unwrap_or(0);
                    let cap = latest + self.proofs_max_blocks_ahead;
                    debug!(
                        target: "derivation",
                        proofs_latest = latest,
                        cap,
                        "Proofs sync gate active"
                    );
                    cap
                }
                Err(e) => {
                    warn!(target: "derivation", error = %e, "Failed to fetch proofs sync status, skipping sync");
                    return Ok(None);
                }
            }
        } else {
            u64::MAX
        };

        let target = remote_head.min(sync_limit);

        if target != remote_head {
            info!(
                target: "derivation",
                sync_limit,
                remote_head,
                "Remote head is ahead of proofs sync limit, capping sync"
            );
        }

        if target <= self.sent_head {
            return Ok(None);
        }

        Ok(Some(target))
    }

    async fn handle_request(
        &self,
        request_type: DerivationActorRequest,
    ) -> Result<(), DerivationError> {
        match request_type {
            DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(safe_head) => {
                debug!(
                    target: "derivation",
                    safe_head = ?*safe_head,
                    "Ignoring engine safe head update in L2 delegate mode"
                );
            }
            DerivationActorRequest::ProcessEngineSyncCompletionRequest(safe_head) => {
                info!(
                    target: "derivation",
                    head = safe_head.block_info.number,
                    "Ignoring engine sync completion in L2 delegate mode"
                );
            }
            DerivationActorRequest::ProcessEngineSignalRequest(_)
            | DerivationActorRequest::ProcessFinalizedL1Block(_)
            | DerivationActorRequest::ProcessL1HeadUpdateRequest(_) => {
                debug!(target: "derivation", request_type = ?request_type, "Ignoring request in L2 delegate mode");
            }
        }
        Ok(())
    }
}

pub(super) struct SyncFromSourceTask<DerivationEngineClient_, L2Source> {
    engine_client: Arc<DerivationEngineClient_>,
    engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
    cancellation_token: CancellationToken,
    local_l2_provider: RootProvider<Base>,
    sent_head: u64,
    target_block: u64,
    l2_source: Arc<L2Source>,
}

impl<DerivationEngineClient_, L2Source> SyncFromSourceTask<DerivationEngineClient_, L2Source>
where
    DerivationEngineClient_: DerivationEngineClient,
    L2Source: L2SourceClient,
{
    pub(super) const fn new(
        engine_client: Arc<DerivationEngineClient_>,
        engine_actor_request_tx: mpsc::Sender<EngineActorRequest>,
        cancellation_token: CancellationToken,
        local_l2_provider: RootProvider<Base>,
        sent_head: u64,
        target_block: u64,
        l2_source: Arc<L2Source>,
    ) -> Self {
        Self {
            engine_client,
            engine_actor_request_tx,
            cancellation_token,
            local_l2_provider,
            sent_head,
            target_block,
            l2_source,
        }
    }

    /// Syncs blocks from the L2 source up to the pre-determined `target_block`.
    ///
    /// Returns the updated `sent_head` on success.
    async fn sync_from_source(&mut self) -> Result<u64, DerivationError> {
        if self.target_block <= self.sent_head {
            return Ok(self.sent_head);
        }

        for block_num in (self.sent_head + 1)..=self.target_block {
            if self.cancellation_token.is_cancelled() {
                info!(target: "derivation", block = block_num, "Sync interrupted by shutdown");
                return Ok(self.sent_head);
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
                .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(payload)))
                .await
                .map_err(|_| {
                    DerivationError::Sender(Box::new(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "engine actor request channel closed",
                    )))
                })?;

            self.sent_head = block_num;
        }

        self.update_safe_and_finalized().await?;

        Ok(self.sent_head)
    }

    async fn update_safe_and_finalized(&self) -> Result<(), DerivationError> {
        let Ok(safe_number) = self.l2_source.get_block_number(BlockNumberOrTag::Safe).await else {
            return Ok(());
        };
        // Delegated labels must never point past blocks we have already forwarded to the local
        // engine, but they must not be clamped to the engine's current safe head. On a fresh
        // follow node the engine safe head starts at genesis, and clamping to it would pin both
        // delegated safe and finalized labels at block 0 forever.
        let local_tip = self.sent_head;
        let clamped_safe = safe_number.min(local_tip);
        let Ok(safe_payload) = self.l2_source.get_payload_by_number(clamped_safe).await else {
            return Ok(());
        };

        let source_hash = safe_payload.execution_payload.block_hash();

        // Detect hash mismatch between source and local EL for the delegated safe block.
        if let Ok(Some(local_block)) =
            self.local_l2_provider.get_block_by_number(clamped_safe.into()).await
            && local_block.header.hash != source_hash
        {
            warn!(
                target: "derivation",
                block_number = clamped_safe,
                local_hash = %local_block.header.hash,
                source_hash = %source_hash,
                "Delegated safe block hash mismatch between source and local EL"
            );
        }

        let safe_l2 = L2BlockInfo {
            block_info: base_protocol::BlockInfo {
                hash: source_hash,
                number: clamped_safe,
                ..Default::default()
            },
            ..Default::default()
        };
        let finalized_l2_number = self
            .l2_source
            .get_block_number(BlockNumberOrTag::Finalized)
            .await
            .ok()
            .map(|number| number.min(local_tip));

        let _ = self
            .engine_client
            .send_delegated_forkchoice_update(DelegatedForkchoiceUpdate {
                safe_l2,
                finalized_l2_number,
            })
            .await;

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
    use mockall::{Sequence, predicate::*};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::derivation::{
        delegate_l2::client::{DelegateL2ClientError, MockL2SourceClient},
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

        let local_l2_provider =
            RootProvider::<Base>::new_http("http://localhost:1234".parse().unwrap());

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

    fn make_sync_task(
        engine_client: MockDerivationEngineClient,
        l2_source: MockL2SourceClient,
        sent_head: u64,
        target_block: u64,
    ) -> (
        SyncFromSourceTask<MockDerivationEngineClient, MockL2SourceClient>,
        mpsc::Receiver<EngineActorRequest>,
        CancellationToken,
    ) {
        let cancel = CancellationToken::new();
        let (engine_tx, engine_rx) = mpsc::channel(16);

        let local_l2_provider =
            RootProvider::<Base>::new_http("http://localhost:1234".parse().unwrap());

        let task = SyncFromSourceTask::new(
            Arc::new(engine_client),
            engine_tx,
            cancel.clone(),
            local_l2_provider,
            sent_head,
            target_block,
            Arc::new(l2_source),
        );

        (task, engine_rx, cancel)
    }

    #[tokio::test]
    async fn handle_sync_completion_enables_sync() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (actor, _, _, _) = make_actor(engine_client, l2_source);

        let safe_head = dummy_l2_block_info(42);
        actor
            .handle_request(DerivationActorRequest::ProcessEngineSyncCompletionRequest(Box::new(
                safe_head,
            )))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn handle_safe_head_update_sets_local_head() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (actor, _, _, _) = make_actor(engine_client, l2_source);

        let safe_head = dummy_l2_block_info(100);
        actor
            .handle_request(DerivationActorRequest::ProcessEngineSafeHeadUpdateRequest(Box::new(
                safe_head,
            )))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn handle_irrelevant_requests_noop() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (actor, _, _, _) = make_actor(engine_client, l2_source);

        actor
            .handle_request(DerivationActorRequest::ProcessL1HeadUpdateRequest(Box::default()))
            .await
            .unwrap();

        actor
            .handle_request(DerivationActorRequest::ProcessFinalizedL1Block(Box::default()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sync_noop_when_target_behind() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();

        let (mut task, _, _) = make_sync_task(engine_client, l2_source, 10, 5);

        let new_head = task.sync_from_source().await.unwrap();
        assert_eq!(new_head, 10);
    }

    #[tokio::test]
    async fn sync_fetches_and_inserts_blocks() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

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

        l2_source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(2));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(2))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(1));

        engine_client.expect_send_delegated_forkchoice_update().returning(|update| {
            assert_eq!(update.safe_l2.block_info.number, 2);
            assert_eq!(update.finalized_l2_number, Some(1));
            Ok(())
        });

        let (mut task, mut engine_rx, _) = make_sync_task(engine_client, l2_source, 0, 3);

        let new_head = task.sync_from_source().await.unwrap();
        assert_eq!(new_head, 3);

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
    async fn delegated_forkchoice_uses_inserted_head_when_engine_safe_head_is_zero() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

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

        l2_source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(2));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(2))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(1));

        engine_client.expect_send_delegated_forkchoice_update().returning(|update| {
            assert_eq!(update.safe_l2.block_info.number, 2);
            assert_eq!(update.finalized_l2_number, Some(1));
            Ok(())
        });

        let (mut task, _engine_rx, _) = make_sync_task(engine_client, l2_source, 0, 3);

        let new_head = task.sync_from_source().await.unwrap();
        assert_eq!(new_head, 3);
    }

    #[tokio::test]
    async fn delegated_forkchoice_not_sent_when_safe_payload_unavailable() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();
        let mut sequence = Sequence::new();

        l2_source
            .expect_get_payload_by_number()
            .with(eq(1))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|n| Ok(dummy_payload_envelope(n)));

        l2_source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(1));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(1))
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|n| Err(DelegateL2ClientError::BlockNotFound(format!("{n}"))));

        engine_client.expect_send_delegated_forkchoice_update().times(0);
        engine_client.expect_send_safe_l2_signal().times(0);
        engine_client.expect_send_finalized_l2_block().times(0);

        let (mut task, mut engine_rx, _) = make_sync_task(engine_client, l2_source, 0, 1);

        let new_head = task.sync_from_source().await.unwrap();
        assert_eq!(new_head, 1);

        let req = engine_rx.try_recv().unwrap();
        assert!(
            matches!(req, EngineActorRequest::ProcessUnsafeL2BlockRequest(_)),
            "expected ProcessUnsafeL2BlockRequest, got {req:?}"
        );
        assert!(engine_rx.is_empty(), "unexpected extra engine requests");
    }

    #[tokio::test]
    async fn sync_aborts_on_cancellation() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();

        let (mut task, engine_rx, cancel) = make_sync_task(engine_client, l2_source, 0, 100);

        cancel.cancel();
        let new_head = task.sync_from_source().await.unwrap();

        assert_eq!(new_head, 0);
        assert!(engine_rx.is_empty());
    }

    #[tokio::test]
    async fn run_loop_stops_on_cancellation() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _deriv_tx, _engine_rx, cancel) = make_actor(engine_client, l2_source);

        actor.sent_head = 10;
        cancel.cancel();

        let result = actor.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_loop_errors_on_channel_close() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, deriv_tx, _engine_rx, _cancel) = make_actor(engine_client, l2_source);

        actor.sent_head = 10;
        drop(deriv_tx);

        let result = actor.run().await;
        assert!(result.is_err());
    }
}
