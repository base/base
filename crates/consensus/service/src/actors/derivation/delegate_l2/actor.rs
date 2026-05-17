use std::{io, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_network::Base;
use base_consensus_engine::{DelegatedForkchoiceUpdate, InsertTaskError};
use base_protocol::L2BlockInfo;
use futures::future::OptionFuture;
use serde::{Deserialize, Serialize};
use tokio::{select, sync::mpsc, task::JoinHandle, time};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tracing::{debug, error, info, warn};

use crate::{
    CancellableContext, DerivationActorRequest, DerivationEngineClient, EngineActorRequest,
    InsertUnsafePayloadRequest, NodeActor,
    actors::derivation::{
        DerivationError,
        delegate_l2::{
            L2SourceClient, PrefetchedL2Block, SourceBlockFetcher, SourceBlockFetcherConfig,
        },
    },
};

const DEFAULT_PROOFS_MAX_BLOCKS_AHEAD: u64 = 512;
const PROOFS_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize, Serialize)]
struct ProofsSyncStatus {
    latest: Option<u64>,
}

/// The [`NodeActor`] for the L2 delegate derivation sub-routine.
///
/// Polls a source L2 execution layer node for new blocks and drives the local
/// engine through an acknowledged unsafe payload insert (`NewPayload` + FCU)
/// rather than running the full derivation pipeline.
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
    source_prefetch_config: SourceBlockFetcherConfig,
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
            source_prefetch_config: SourceBlockFetcherConfig::default(),
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

    /// Sets the source block prefetcher configuration.
    pub const fn with_source_prefetch_config(mut self, config: SourceBlockFetcherConfig) -> Self {
        self.source_prefetch_config = config;
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

        let mut source_prefetcher = self.start_source_prefetcher(self.sent_head.saturating_add(1));
        let mut pending_block: Option<PrefetchedL2Block> = None;
        let mut insert_limit = if self.proofs_enabled { 0 } else { u64::MAX };
        let mut proofs_ticker =
            time::interval_at(time::Instant::now() + PROOFS_POLL_INTERVAL, PROOFS_POLL_INTERVAL);
        proofs_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let mut delegated_forkchoice_ticker =
            time::interval_at(time::Instant::now() + Self::POLL_INTERVAL, Self::POLL_INTERVAL);
        delegated_forkchoice_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

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
                Some(result) = OptionFuture::from(source_prefetcher.task.as_mut()) => {
                    source_prefetcher.task = None;
                    match result {
                        Err(join_error) => {
                            warn!(target: "derivation", error = %join_error, "Source block prefetcher stopped unexpectedly");
                        }
                        Ok(Err(derivation_error)) => {
                            warn!(target: "derivation", error = %derivation_error, "Source block prefetcher failed");
                        }
                        Ok(Ok(())) => {
                            warn!(target: "derivation", sent_head = self.sent_head, "Source block prefetcher exited");
                        }
                    }
                    self.restart_source_prefetcher(&mut source_prefetcher, &mut pending_block).await?;
                }
                _ = delegated_forkchoice_ticker.tick() => {
                    self.update_safe_and_finalized().await?;
                }
                _ = proofs_ticker.tick(), if pending_block.is_some() => {
                    if matches!(
                        self.try_insert_pending_block(&mut pending_block, &mut insert_limit).await?,
                        PendingInsertOutcome::Restart
                    ) {
                        self.restart_source_prefetcher(&mut source_prefetcher, &mut pending_block).await?;
                    }
                }
                prefetched = source_prefetcher.rx.recv(), if pending_block.is_none() => {
                    let Some(prefetched) = prefetched else {
                        warn!(target: "derivation", sent_head = self.sent_head, "Source block prefetch queue closed");
                        self.restart_source_prefetcher(&mut source_prefetcher, &mut pending_block).await?;
                        continue;
                    };
                    pending_block = Some(prefetched);
                    if matches!(
                        self.try_insert_pending_block(&mut pending_block, &mut insert_limit).await?,
                        PendingInsertOutcome::Restart
                    ) {
                        self.restart_source_prefetcher(&mut source_prefetcher, &mut pending_block).await?;
                    }
                }
            }
        }
    }

    fn start_source_prefetcher(&self, start_number: u64) -> SourcePrefetcher {
        let buffer_blocks = self.source_prefetch_config.buffer_blocks.max(1);
        let (tx, rx) = mpsc::channel(buffer_blocks);
        let cancellation_token = self.cancellation_token.child_token();
        let fetcher = SourceBlockFetcher::new(
            Arc::clone(&self.l2_source),
            start_number,
            tx,
            cancellation_token.clone(),
            self.source_prefetch_config,
        );
        let task = Some(tokio::spawn(async move { fetcher.run().await }));

        SourcePrefetcher { cancellation_token, rx, task }
    }

    async fn restart_source_prefetcher(
        &mut self,
        source_prefetcher: &mut SourcePrefetcher,
        pending_block: &mut Option<PrefetchedL2Block>,
    ) -> Result<(), DerivationError> {
        source_prefetcher.cancellation_token.cancel();
        if let Some(task) = source_prefetcher.task.take() {
            task.abort();
        }
        while source_prefetcher.rx.try_recv().is_ok() {}
        *pending_block = None;

        let local_head = self
            .local_l2_provider
            .get_block_number()
            .await
            .map_err(|e| DerivationError::Sender(Box::new(e)))?;
        self.sent_head = local_head;
        let start_number = self.sent_head.saturating_add(1);
        info!(
            target: "derivation",
            sent_head = self.sent_head,
            start_number,
            "Restarting source block prefetcher"
        );
        *source_prefetcher = self.start_source_prefetcher(start_number);

        Ok(())
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

    async fn try_insert_pending_block(
        &mut self,
        pending_block: &mut Option<PrefetchedL2Block>,
        insert_limit: &mut u64,
    ) -> Result<PendingInsertOutcome, DerivationError> {
        let Some(block) = pending_block.as_ref() else {
            return Ok(PendingInsertOutcome::Idle);
        };

        let expected_number = self.sent_head.saturating_add(1);
        if block.number != expected_number {
            warn!(
                target: "derivation",
                expected_number,
                actual_number = block.number,
                sent_head = self.sent_head,
                "Discarding stale prefetched source block"
            );
            return Ok(PendingInsertOutcome::Restart);
        }

        if !self.block_allowed_by_proofs(block.number, insert_limit).await {
            debug!(
                target: "derivation",
                block_number = block.number,
                insert_limit = *insert_limit,
                "Waiting for proofs gate before inserting source block"
            );
            return Ok(PendingInsertOutcome::Blocked);
        }

        let block = pending_block.take().expect("pending block exists");
        self.insert_prefetched_block(block).await
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

    async fn block_allowed_by_proofs(&self, block_number: u64, insert_limit: &mut u64) -> bool {
        if !self.proofs_enabled {
            return true;
        }

        if block_number <= *insert_limit {
            return true;
        }

        match self
            .local_l2_provider
            .raw_request::<_, ProofsSyncStatus>("debug_proofsSyncStatus".into(), ())
            .await
        {
            Ok(status) => {
                let proofs_latest = status.latest.unwrap_or(0);
                *insert_limit = proofs_latest.saturating_add(self.proofs_max_blocks_ahead);
                debug!(
                    target: "derivation",
                    proofs_latest,
                    insert_limit = *insert_limit,
                    "Proofs sync gate refreshed"
                );
                block_number <= *insert_limit
            }
            Err(err) => {
                warn!(
                    target: "derivation",
                    block_number,
                    error = %err,
                    "Failed to fetch proofs sync status"
                );
                false
            }
        }
    }

    async fn insert_prefetched_block(
        &mut self,
        block: PrefetchedL2Block,
    ) -> Result<PendingInsertOutcome, DerivationError> {
        debug!(
            target: "derivation",
            block_number = block.number,
            block_hash = %block.hash(),
            "Inserting prefetched block from L2 source"
        );

        let expected_number = block.number;
        let expected_hash = block.hash();
        let (result_tx, mut result_rx) = mpsc::channel(1);

        self.engine_actor_request_tx
            .send(EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(Box::new(
                InsertUnsafePayloadRequest { envelope: block.envelope, result_tx: Some(result_tx) },
            )))
            .await
            .map_err(|_| {
                DerivationError::Sender(Box::new(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "engine actor request channel closed",
                )))
            })?;

        let inserted_head = match result_rx.recv().await {
            Some(Ok(inserted_head)) => inserted_head,
            Some(Err(err)) => return Ok(self.handle_insert_error(expected_number, err)),
            None => {
                return Err(DerivationError::Sender(Box::new(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "engine insert result channel closed",
                ))));
            }
        };

        if inserted_head.block_info.number != expected_number
            || inserted_head.block_info.hash != expected_hash
        {
            warn!(
                target: "derivation",
                expected_number,
                expected_hash = %expected_hash,
                actual_number = inserted_head.block_info.number,
                actual_hash = %inserted_head.block_info.hash,
                "Engine inserted unexpected source block"
            );
            return Ok(PendingInsertOutcome::Restart);
        }

        self.sent_head = expected_number;
        Ok(PendingInsertOutcome::Inserted)
    }

    fn handle_insert_error(&self, block_number: u64, err: InsertTaskError) -> PendingInsertOutcome {
        warn!(
            target: "derivation",
            block_number,
            error = %err,
            "Engine failed to insert prefetched source block"
        );
        PendingInsertOutcome::Restart
    }
}

#[derive(Debug)]
struct SourcePrefetcher {
    cancellation_token: CancellationToken,
    rx: mpsc::Receiver<PrefetchedL2Block>,
    task: Option<JoinHandle<Result<(), DerivationError>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingInsertOutcome {
    Idle,
    Blocked,
    Inserted,
    Restart,
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_consensus_engine::InsertTaskError;
    use base_protocol::{BlockInfo, L2BlockInfo};
    use jsonrpsee::{RpcModule, core::to_json_value, server::ServerHandle};
    use mockall::predicate::*;
    use tokio::{sync::mpsc, time::timeout};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::derivation::{
        delegate_l2::client::MockL2SourceClient, engine_client::MockDerivationEngineClient,
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

    fn dummy_payload_envelope(block_number: u64) -> BaseExecutionPayloadEnvelope {
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
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(payload),
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

    async fn recv_follow_insert(
        engine_rx: &mut mpsc::Receiver<EngineActorRequest>,
        expected_num: u64,
    ) -> mpsc::Sender<Result<L2BlockInfo, InsertTaskError>> {
        let req = engine_rx.recv().await.unwrap();
        match req {
            EngineActorRequest::ProcessLocalUnsafeL2BlockRequest(request) => {
                assert_eq!(request.envelope.execution_payload.block_number(), expected_num);
                assert_eq!(
                    request.envelope.execution_payload.block_hash(),
                    dummy_l2_block_info(expected_num).block_info.hash
                );
                request.result_tx.unwrap()
            }
            other => panic!("Expected ProcessLocalUnsafeL2BlockRequest, got {other:?}"),
        }
    }

    async fn ack_follow_insert(
        engine_rx: &mut mpsc::Receiver<EngineActorRequest>,
        expected_num: u64,
    ) {
        recv_follow_insert(engine_rx, expected_num)
            .await
            .send(Ok(dummy_l2_block_info(expected_num)))
            .await
            .unwrap();
    }

    async fn proofs_status_provider(latest: u64) -> (RootProvider<Base>, ServerHandle) {
        let mut module = RpcModule::new(());
        let status = to_json_value(ProofsSyncStatus { latest: Some(latest) }).unwrap();
        module.register_method("debug_proofsSyncStatus", move |_, _, _| status.clone()).unwrap();

        let server = jsonrpsee::server::Server::builder()
            .build(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.start(module);
        let provider = RootProvider::<Base>::new_http(format!("http://{addr}").parse().unwrap());

        (provider, handle)
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
    async fn source_fetcher_emits_ordered_blocks_until_remote_head() {
        let mut l2_source = MockL2SourceClient::new();

        l2_source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(3));
        l2_source.expect_get_payload_by_number().returning(|n| Ok(dummy_payload_envelope(n)));

        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(4);
        let fetcher = SourceBlockFetcher::new(
            Arc::new(l2_source),
            1,
            tx,
            cancel.clone(),
            SourceBlockFetcherConfig {
                head_poll_interval: Duration::from_secs(60),
                ..SourceBlockFetcherConfig::default()
            },
        );
        let handle = tokio::spawn(async move { fetcher.run().await });

        for expected in 1..=3 {
            let block = timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("timed out waiting for prefetched block")
                .expect("prefetch channel closed");
            assert_eq!(block.number, expected);
            assert_eq!(block.envelope.execution_payload.block_number(), expected);
        }

        cancel.cancel();
        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn follow_prefetches_next_block_before_prior_insert_ack() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();
        let (fetched_12_tx, mut fetched_12_rx) = mpsc::unbounded_channel();

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(12));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(11))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source.expect_get_payload_by_number().with(eq(12)).returning(move |n| {
            let _ = fetched_12_tx.send(());
            Ok(dummy_payload_envelope(n))
        });

        engine_client.expect_send_delegated_forkchoice_update().times(0);
        engine_client.expect_send_safe_l2_signal().times(0);
        engine_client.expect_send_finalized_l2_block().times(0);

        let (mut actor, _deriv_tx, mut engine_rx, cancel) = make_actor(engine_client, l2_source);
        actor.sent_head = 10;
        actor.source_prefetch_config = SourceBlockFetcherConfig {
            head_poll_interval: Duration::from_secs(60),
            ..SourceBlockFetcherConfig::default()
        };
        let actor_handle = tokio::spawn(async move { actor.run().await });

        let first_result_tx = recv_follow_insert(&mut engine_rx, 11).await;
        timeout(Duration::from_millis(100), fetched_12_rx.recv())
            .await
            .expect("block 12 was not prefetched before block 11 ack")
            .expect("fetch signal channel closed");
        assert!(
            timeout(Duration::from_millis(20), engine_rx.recv()).await.is_err(),
            "follow mode sent another insert before the prior insert was acknowledged"
        );

        first_result_tx.send(Ok(dummy_l2_block_info(11))).await.unwrap();
        ack_follow_insert(&mut engine_rx, 12).await;
        cancel.cancel();

        assert!(actor_handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn proofs_gate_blocks_insertion_without_stopping_source_prefetch() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();
        let (fetched_12_tx, mut fetched_12_rx) = mpsc::unbounded_channel();
        let (local_l2_provider, _proofs_server) = proofs_status_provider(10).await;

        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(12));
        l2_source
            .expect_get_payload_by_number()
            .with(eq(11))
            .returning(|n| Ok(dummy_payload_envelope(n)));
        l2_source.expect_get_payload_by_number().with(eq(12)).returning(move |n| {
            let _ = fetched_12_tx.send(());
            Ok(dummy_payload_envelope(n))
        });

        engine_client.expect_send_delegated_forkchoice_update().times(0);
        engine_client.expect_send_safe_l2_signal().times(0);
        engine_client.expect_send_finalized_l2_block().times(0);

        let (mut actor, _deriv_tx, mut engine_rx, cancel) = make_actor(engine_client, l2_source);
        actor.local_l2_provider = local_l2_provider;
        actor.sent_head = 10;
        actor.proofs_enabled = true;
        actor.proofs_max_blocks_ahead = 0;
        actor.source_prefetch_config = SourceBlockFetcherConfig {
            head_poll_interval: Duration::from_secs(60),
            ..SourceBlockFetcherConfig::default()
        };
        let actor_handle = tokio::spawn(async move { actor.run().await });

        timeout(Duration::from_millis(100), fetched_12_rx.recv())
            .await
            .expect("block 12 was not prefetched while block 11 was proofs-gated")
            .expect("fetch signal channel closed");
        assert!(
            timeout(Duration::from_millis(20), engine_rx.recv()).await.is_err(),
            "proofs-gated block was sent to the engine before proofs advanced"
        );

        cancel.cancel();
        assert!(timeout(Duration::from_secs(1), actor_handle).await.unwrap().unwrap().is_ok());
    }

    #[tokio::test]
    async fn controller_holds_block_until_previous_insert_ack() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _, mut engine_rx, _) = make_actor(engine_client, l2_source);
        let mut insert_limit = u64::MAX;
        actor.sent_head = 10;

        let mut pending_block =
            Some(PrefetchedL2Block { number: 11, envelope: dummy_payload_envelope(11) });
        let insert = tokio::spawn(async move {
            actor.try_insert_pending_block(&mut pending_block, &mut insert_limit).await
        });

        let first_result_tx = recv_follow_insert(&mut engine_rx, 11).await;
        assert!(
            timeout(Duration::from_millis(20), insert).await.is_err(),
            "insert completed before engine ack"
        );

        first_result_tx.send(Ok(dummy_l2_block_info(11))).await.unwrap();
    }

    #[tokio::test]
    async fn insert_failure_requests_prefetch_restart() {
        let engine_client = MockDerivationEngineClient::new();
        let l2_source = MockL2SourceClient::new();
        let (mut actor, _, mut engine_rx, _) = make_actor(engine_client, l2_source);
        let mut insert_limit = u64::MAX;
        actor.sent_head = 10;

        let mut pending_block =
            Some(PrefetchedL2Block { number: 11, envelope: dummy_payload_envelope(11) });
        let insert = tokio::spawn(async move {
            actor.try_insert_pending_block(&mut pending_block, &mut insert_limit).await
        });

        recv_follow_insert(&mut engine_rx, 11)
            .await
            .send(Err(InsertTaskError::ForkchoiceUpdateDidNotAdvance))
            .await
            .unwrap();

        let outcome = insert.await.unwrap().unwrap();
        assert_eq!(outcome, PendingInsertOutcome::Restart);
    }

    #[tokio::test]
    async fn delegated_forkchoice_uses_inserted_head_when_engine_safe_head_is_zero() {
        let mut engine_client = MockDerivationEngineClient::new();
        let mut l2_source = MockL2SourceClient::new();

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

        let (actor, _, _, _) = make_actor(engine_client, l2_source);
        let mut actor = actor;
        actor.sent_head = 3;
        actor.update_safe_and_finalized().await.unwrap();
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
        let mut l2_source = MockL2SourceClient::new();
        l2_source
            .expect_get_block_number()
            .with(eq(BlockNumberOrTag::Latest))
            .returning(|_| Ok(10));
        let (mut actor, deriv_tx, _engine_rx, _cancel) = make_actor(engine_client, l2_source);

        actor.sent_head = 10;
        drop(deriv_tx);

        let result = actor.run().await;
        assert!(result.is_err());
    }
}
