use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::{
    sync::mpsc,
    time::{self, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    Metrics,
    follow::{
        engine::FollowEngine,
        error::FollowError,
        local::FollowLocalClient,
        prefetcher::{PREFETCH_WINDOW, PayloadPrefetcher, PrefetchedPayload},
        proof_gate::ProofGate,
        recovery::FollowRecovery,
        source::RemoteClient,
    },
};

const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SafetyUpdate {
    Updated,
    Recover,
}

#[derive(Debug, PartialEq, Eq)]
enum GenerationOutcome {
    Stopped,
    Recover,
}

#[derive(Debug)]
pub(super) struct FollowRuntime<Local, Remote, Gate> {
    local: Arc<Local>,
    source: Arc<Remote>,
    engine: Arc<dyn FollowEngine>,
    cancellation: CancellationToken,
    follow_from_block: L2BlockInfo,
    proof_gate: Gate,
    insert_delay: Duration,
}

impl<Local, Remote, Gate> FollowRuntime<Local, Remote, Gate>
where
    Local: FollowLocalClient + 'static,
    Remote: RemoteClient + 'static,
    Gate: ProofGate + 'static,
{
    pub(super) fn new(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
        cancellation: CancellationToken,
        follow_from_block: L2BlockInfo,
        proof_gate: Gate,
        insert_delay: Duration,
    ) -> Self {
        Self { local, source, engine, cancellation, follow_from_block, proof_gate, insert_delay }
    }

    async fn run_ordered_insert_loop<GateInner: ProofGate>(
        engine: Arc<dyn FollowEngine>,
        cancellation: CancellationToken,
        mut blocks_to_insert_rx: mpsc::Receiver<PrefetchedPayload>,
        start_block: u64,
        proof_gate: &mut GateInner,
        insert_delay: Duration,
    ) -> Result<(), FollowError> {
        let mut current_block = start_block;

        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }

            proof_gate.wait_til_ready(current_block).await?;

            let Some(payload) = blocks_to_insert_rx.recv().await else {
                return Err(FollowError::BlocksToInsertChannelClosed);
            };
            let block_number = payload.execution_payload.block_number();
            if block_number != current_block {
                return Err(FollowError::OutOfOrderPayload {
                    actual: block_number,
                    expected: current_block,
                });
            }

            info!(target: "follow", block = current_block, "Inserting source payload");
            engine.insert_payload(payload).await?;
            if !insert_delay.is_zero() {
                debug!(
                    target: "follow",
                    block = current_block,
                    delay = ?insert_delay,
                    "Sleeping after source payload insert"
                );
                time::sleep(insert_delay).await;
            }
            current_block = current_block.saturating_add(1);
        }
    }

    async fn update_safe_and_finalized(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
    ) -> Result<SafetyUpdate, FollowError> {
        let latest = local
            .block_info(BlockNumberOrTag::Latest)
            .await?
            .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Latest))?;
        let Some(local_safe) = local.block_info(BlockNumberOrTag::Safe).await? else {
            debug!(target: "follow", "Skipping safe/finalized update because local safe label is unavailable");
            return Ok(SafetyUpdate::Updated);
        };
        let local_finalized = local.block_info(BlockNumberOrTag::Finalized).await?;

        // Read number and hash from the same response, matching op-node's `L2BlockRefByLabel`.
        let source_safe = source.get_block_info(BlockNumberOrTag::Safe).await?;
        let source_finalized = source.get_block_info(BlockNumberOrTag::Finalized).await?;

        // Check finalized before safe recovery. A finalized disagreement is never recoverable and
        // must only alert while ordinary following remains active.
        let local_finalized_number =
            local_finalized.map(|block| block.block_info.number).unwrap_or_default();
        let finalized_ceiling =
            latest.block_info.number.min(source_safe.number.max(local_safe.block_info.number));
        let finalized = match Self::verified_local_block(
            &local,
            &source_finalized,
            local_finalized_number,
            finalized_ceiling,
        )
        .await
        {
            Ok(finalized) => finalized,
            Err(FollowError::SourceBlockHashMismatch { number, local, remote }) => {
                return Err(FollowError::FinalizedDivergence { number, local, remote });
            }
            Err(error) => return Err(error),
        };
        if let Some(finalized_block) = finalized.as_ref() {
            Self::log_l2_origin_validation(local.as_ref(), finalized_block).await;
        }

        let safe = match Self::verified_local_block(
            &local,
            &source_safe,
            local_safe.block_info.number,
            latest.block_info.number,
        )
        .await
        {
            Ok(safe) => safe,
            Err(FollowError::SourceBlockHashMismatch { .. }) => {
                return Ok(SafetyUpdate::Recover);
            }
            Err(error) => return Err(error),
        };
        if let Some(safe_block) = safe.as_ref() {
            Self::log_l2_origin_validation(local.as_ref(), safe_block).await;
        }

        engine.update_safe_finalized_blocks(safe, finalized).await?;
        Ok(SafetyUpdate::Updated)
    }

    /// Returns the local block at a coherent source label's height (`{number, hash}` from a single
    /// read) when the number is within `[floor, ceiling]` and the local hash matches the source
    /// hash. Labels outside the local range, or missing locally, are skipped. In-range hash
    /// divergence returns `SourceBlockHashMismatch`.
    async fn verified_local_block(
        local: &Local,
        source_block: &BlockInfo,
        floor: u64,
        ceiling: u64,
    ) -> Result<Option<L2BlockInfo>, FollowError> {
        let number = source_block.number;
        if number < floor || number > ceiling {
            debug!(
                target: "follow",
                number,
                floor,
                ceiling,
                hash = %source_block.hash,
                "Skipping source label outside local range"
            );
            return Ok(None);
        }
        let Some(local_block) = local.block_info(number.into()).await? else {
            return Ok(None);
        };
        if local_block.block_info.hash != source_block.hash {
            return Err(FollowError::SourceBlockHashMismatch {
                number,
                local: local_block.block_info.hash,
                remote: source_block.hash,
            });
        }
        Ok(Some(local_block))
    }

    /// Checks whether a hash-verified local block's L1 origin is canonical in the local L1 view.
    async fn validate_l2_origin_against_local_l1(
        local: &Local,
        block: &L2BlockInfo,
    ) -> Result<(), FollowError> {
        let origin = block.l1_origin;
        let local_hash = local
            .l1_block_hash(origin.number)
            .await?
            .ok_or(FollowError::LocalL1BlockUnavailable(origin.number))?;
        if local_hash != origin.hash {
            return Err(FollowError::L2OriginNotCanonical {
                l2_number: block.block_info.number,
                l1_number: origin.number,
                local_l1: local_hash,
                l2_origin: origin.hash,
            });
        }
        Ok(())
    }

    async fn log_l2_origin_validation(local: &Local, block: &L2BlockInfo) {
        match Self::validate_l2_origin_against_local_l1(local, block).await {
            Ok(()) => {}
            Err(FollowError::LocalL1BlockUnavailable(l1_number)) => {
                Metrics::follow_l1_origin_check_failures_total("unavailable").increment(1);
                info!(
                    target: "follow",
                    l2_block = block.block_info.number,
                    l1_block = l1_number,
                    "Local L1 origin block unavailable; promoting label without local L1 confirmation"
                );
            }
            Err(FollowError::LocalL1BlockFetch { number, source }) => {
                Metrics::follow_l1_origin_check_failures_total("fetch_failed").increment(1);
                info!(
                    target: "follow",
                    error = %source,
                    l2_block = block.block_info.number,
                    l1_block = number,
                    "L1 origin fetch failed; promoting label without local L1 confirmation"
                );
            }
            Err(error) => {
                Metrics::follow_l1_origin_check_failures_total("not_canonical").increment(1);
                warn!(
                    target: "follow",
                    error = %error,
                    l2_block = block.block_info.number,
                    "L2 origin not canonical on local L1; promoting label anyway"
                );
            }
        }
    }
}

impl<Local, Remote, Gate> FollowRuntime<Local, Remote, Gate>
where
    Local: FollowLocalClient + 'static,
    Remote: RemoteClient + 'static,
    Gate: ProofGate + 'static,
{
    async fn run_generation(&mut self, start_block: u64) -> Result<GenerationOutcome, FollowError> {
        let generation_cancellation = self.cancellation.child_token();
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        let prefetcher = PayloadPrefetcher::new(
            Arc::clone(&self.source),
            generation_cancellation.clone(),
            blocks_to_insert_tx,
        );
        let fetch_loop = prefetcher.run(start_block);
        let insert_loop = Self::run_ordered_insert_loop(
            Arc::clone(&self.engine),
            generation_cancellation.clone(),
            blocks_to_insert_rx,
            start_block.saturating_add(1),
            &mut self.proof_gate,
            self.insert_delay,
        );
        let mut ticker =
            time::interval_at(time::Instant::now() + SAFETY_POLL_INTERVAL, SAFETY_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tokio::pin!(fetch_loop);
        tokio::pin!(insert_loop);

        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => {
                    generation_cancellation.cancel();
                    return Ok(GenerationOutcome::Stopped);
                }
                result = &mut fetch_loop => {
                    generation_cancellation.cancel();
                    result?;
                    return Ok(GenerationOutcome::Stopped);
                }
                result = &mut insert_loop => {
                    generation_cancellation.cancel();
                    result?;
                    return Ok(GenerationOutcome::Stopped);
                }
                _ = ticker.tick() => {
                    match Self::update_safe_and_finalized(
                        Arc::clone(&self.local),
                        Arc::clone(&self.source),
                        Arc::clone(&self.engine),
                    )
                    .await
                    {
                        Ok(SafetyUpdate::Updated) => {}
                        Ok(SafetyUpdate::Recover) => {
                            warn!(
                                target: "follow",
                                "Detected safe-head divergence; pausing payload insertion"
                            );
                            generation_cancellation.cancel();
                            return Ok(GenerationOutcome::Recover);
                        }
                        Err(error @ FollowError::FinalizedDivergence { .. }) => {
                            error!(
                                target: "follow",
                                error = %error,
                                "Detected finalized-head divergence"
                            );
                        }
                        Err(error) => {
                            warn!(
                                target: "follow",
                                error = %error,
                                "Failed to update safe/finalized labels"
                            );
                        }
                    }
                }
            }
        }
    }

    pub(super) async fn start(mut self) -> Result<(), FollowError> {
        let mut start_block = self.follow_from_block.block_info.number;
        let mut needs_recovery = match Self::update_safe_and_finalized(
            Arc::clone(&self.local),
            Arc::clone(&self.source),
            Arc::clone(&self.engine),
        )
        .await
        {
            Ok(SafetyUpdate::Recover) => true,
            Ok(SafetyUpdate::Updated) => false,
            Err(error @ FollowError::FinalizedDivergence { .. }) => {
                error!(
                    target: "follow",
                    error = %error,
                    "Detected finalized-head divergence"
                );
                false
            }
            Err(error) => {
                warn!(
                    target: "follow",
                    error = %error,
                    "Failed initial safe/finalized update"
                );
                false
            }
        };

        loop {
            if self.cancellation.is_cancelled() {
                return Ok(());
            }

            if needs_recovery {
                match FollowRecovery::recover(
                    Arc::clone(&self.local),
                    Arc::clone(&self.source),
                    Arc::clone(&self.engine),
                    self.cancellation.clone(),
                )
                .await
                {
                    Ok(recovered) => {
                        start_block = recovered.block_info.number;
                        needs_recovery = false;
                        info!(
                            target: "follow",
                            number = start_block,
                            hash = %recovered.block_info.hash,
                            "Safe-head recovery completed; restarting payload insertion"
                        );
                    }
                    Err(FollowError::RecoveryCancelled) if self.cancellation.is_cancelled() => {
                        return Ok(());
                    }
                    Err(error @ FollowError::FinalizedDivergence { .. }) => {
                        error!(
                            target: "follow",
                            error = %error,
                            "Safe-head recovery hit finalized divergence; keeping payload insertion paused"
                        );
                        time::sleep(SAFETY_POLL_INTERVAL).await;
                    }
                    Err(error) => {
                        warn!(
                            target: "follow",
                            error = %error,
                            "Safe-head recovery failed; re-reading labels before retry"
                        );
                        time::sleep(SAFETY_POLL_INTERVAL).await;
                    }
                }
                continue;
            }

            match self.run_generation(start_block).await? {
                GenerationOutcome::Stopped => return Ok(()),
                GenerationOutcome::Recover => needs_recovery = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use alloy_primitives::B256;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use async_trait::async_trait;
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
    use base_protocol::{BlockInfo, L2BlockInfo};
    use mockall::predicate::eq;
    use tokio::{sync::Mutex, time};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        MockRemoteClient,
        follow::{
            engine::{FollowEngine, MockFollowEngine},
            local::MockFollowLocalClient,
            proof_gate::{ActiveProofGate, NoopProofGate},
        },
    };

    const DEFAULT_PROOFS_MAX_BLOCKS_AHEAD: u64 = 16;

    #[derive(Debug)]
    struct RecordingEngine {
        inserted: Mutex<Vec<u64>>,
        labels: Mutex<Vec<(Option<u64>, Option<u64>)>>,
        delay: Duration,
    }

    #[derive(Debug)]
    struct DelayedSource {
        latest: u64,
        fetch_delay: Duration,
    }

    #[derive(Debug)]
    struct RecoveryRecordingEngine {
        inserted: Mutex<Vec<u64>>,
        forkchoices: Mutex<Vec<(BlockInfo, BlockInfo, BlockInfo)>>,
        labels: Mutex<Vec<(Option<u64>, Option<u64>)>>,
        head_requests: AtomicU64,
    }

    #[derive(Debug)]
    struct PauseFirstGeneration {
        first_wait: bool,
    }

    #[async_trait]
    impl ProofGate for PauseFirstGeneration {
        async fn wait_til_ready(&mut self, _current_block: u64) -> Result<(), FollowError> {
            if self.first_wait {
                self.first_wait = false;
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl RemoteClient for DelayedSource {
        async fn get_block_number(
            &self,
            tag: BlockNumberOrTag,
        ) -> Result<u64, crate::RemoteL2ClientError> {
            match tag {
                BlockNumberOrTag::Latest => Ok(self.latest),
                BlockNumberOrTag::Number(number) => Ok(number),
                _ => Ok(0),
            }
        }

        async fn get_block_info(
            &self,
            tag: BlockNumberOrTag,
        ) -> Result<BlockInfo, crate::RemoteL2ClientError> {
            Ok(match tag {
                BlockNumberOrTag::Latest => source_block_info(self.latest),
                BlockNumberOrTag::Number(number) => source_block_info(number),
                _ => source_block_info(0),
            })
        }

        async fn get_block_info_by_hash(
            &self,
            hash: B256,
        ) -> Result<BlockInfo, crate::RemoteL2ClientError> {
            Ok(source_block_info(u64::from(hash.as_slice()[31])))
        }

        async fn get_payload_by_number(
            &self,
            number: u64,
        ) -> Result<BaseExecutionPayloadEnvelope, crate::RemoteL2ClientError> {
            time::sleep(self.fetch_delay).await;
            Ok(payload(number))
        }
    }

    #[async_trait]
    impl FollowEngine for RecordingEngine {
        async fn insert_payload(
            &self,
            envelope: BaseExecutionPayloadEnvelope,
        ) -> Result<(), FollowError> {
            time::sleep(self.delay).await;
            self.inserted.lock().await.push(envelope.execution_payload.block_number());
            Ok(())
        }

        async fn update_safe_finalized_blocks(
            &self,
            safe: Option<L2BlockInfo>,
            finalized: Option<L2BlockInfo>,
        ) -> Result<(), FollowError> {
            // Mirror the real engine: a no-op when there is nothing to update.
            if safe.is_none() && finalized.is_none() {
                return Ok(());
            }
            self.labels
                .lock()
                .await
                .push((safe.map(|v| v.block_info.number), finalized.map(|v| v.block_info.number)));
            Ok(())
        }

        async fn request_forkchoice(
            &self,
            _head: BlockInfo,
            _safe: BlockInfo,
            _finalized: BlockInfo,
        ) -> Result<bool, FollowError> {
            Ok(true)
        }
    }

    #[async_trait]
    impl FollowEngine for RecoveryRecordingEngine {
        async fn insert_payload(
            &self,
            envelope: BaseExecutionPayloadEnvelope,
        ) -> Result<(), FollowError> {
            self.inserted.lock().await.push(envelope.execution_payload.block_number());
            Ok(())
        }

        async fn update_safe_finalized_blocks(
            &self,
            safe: Option<L2BlockInfo>,
            finalized: Option<L2BlockInfo>,
        ) -> Result<(), FollowError> {
            self.labels
                .lock()
                .await
                .push((safe.map(|v| v.block_info.number), finalized.map(|v| v.block_info.number)));
            Ok(())
        }

        async fn request_forkchoice(
            &self,
            head: BlockInfo,
            safe: BlockInfo,
            finalized: BlockInfo,
        ) -> Result<bool, FollowError> {
            self.forkchoices.lock().await.push((head, safe, finalized));
            Ok(self.head_requests.fetch_add(1, Ordering::SeqCst) > 0)
        }
    }

    fn block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: base_protocol::BlockInfo {
                number,
                hash: B256::from([number as u8; 32]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn source_block_info(number: u64) -> BlockInfo {
        BlockInfo {
            number,
            hash: B256::from([number as u8; 32]),
            parent_hash: if number == 0 {
                B256::ZERO
            } else {
                B256::from([number.saturating_sub(1) as u8; 32])
            },
            ..Default::default()
        }
    }

    fn payload(number: u64) -> BaseExecutionPayloadEnvelope {
        BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: None,
            execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: alloy_primitives::Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: alloy_primitives::Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: number,
                gas_limit: 0,
                gas_used: 0,
                timestamp: 0,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: B256::from([number as u8; 32]),
                transactions: vec![],
            }),
        }
    }

    fn local_client(
        latest: u64,
        safe: u64,
        finalized: u64,
        proofs_latest: u64,
    ) -> MockFollowLocalClient {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(move |tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest => block_info(latest),
                BlockNumberOrTag::Safe => block_info(safe),
                BlockNumberOrTag::Finalized => block_info(finalized),
                BlockNumberOrTag::Number(number) => block_info(number),
                _ => block_info(0),
            }))
        });
        local.expect_l1_block_hash().returning(|_| Ok(Some(B256::ZERO)));
        local.expect_proofs_latest().returning(move || Ok(Some(proofs_latest)));
        local
    }

    #[tokio::test]
    async fn ordered_insertion_consumes_channel_order() {
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let mut proof_gate = NoopProofGate;
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        blocks_to_insert_tx.send(payload(1)).await.expect("send 1");
        blocks_to_insert_tx.send(payload(2)).await.expect("send 2");
        blocks_to_insert_tx.send(payload(3)).await.expect("send 3");
        drop(blocks_to_insert_tx);

        let engine_for_loop: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let error = FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::run_ordered_insert_loop(
            engine_for_loop,
            CancellationToken::new(),
            blocks_to_insert_rx,
            1,
            &mut proof_gate,
            Duration::ZERO,
        )
        .await
        .expect_err("closed channel");

        assert_eq!(*engine.inserted.lock().await, vec![1, 2, 3]);
        assert!(matches!(error, FollowError::BlocksToInsertChannelClosed));
    }

    #[tokio::test]
    async fn ordered_insertion_rejects_out_of_order_channel_input() {
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let mut proof_gate = NoopProofGate;
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        blocks_to_insert_tx.send(payload(2)).await.expect("send 2");
        drop(blocks_to_insert_tx);

        let error = FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::run_ordered_insert_loop(
            engine,
            CancellationToken::new(),
            blocks_to_insert_rx,
            1,
            &mut proof_gate,
            Duration::ZERO,
        )
        .await
        .expect_err("error");

        assert!(matches!(error, FollowError::OutOfOrderPayload { actual: 2, expected: 1 }));
    }

    #[tokio::test]
    async fn ordered_insertion_applies_configured_insert_delay() {
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let mut proof_gate = NoopProofGate;
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        blocks_to_insert_tx.send(payload(1)).await.expect("send 1");
        blocks_to_insert_tx.send(payload(2)).await.expect("send 2");
        drop(blocks_to_insert_tx);

        let engine_for_loop: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let started = Instant::now();
        let error = FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::run_ordered_insert_loop(
            engine_for_loop,
            CancellationToken::new(),
            blocks_to_insert_rx,
            1,
            &mut proof_gate,
            Duration::from_millis(20),
        )
        .await
        .expect_err("closed channel");

        assert!(matches!(error, FollowError::BlocksToInsertChannelClosed));
        assert_eq!(*engine.inserted.lock().await, vec![1, 2]);
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn prefetch_backpressures_on_bounded_channel() {
        let requests = Arc::new(AtomicU64::new(0));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(100));
        let requests_for_mock = Arc::clone(&requests);
        source.expect_get_payload_by_number().returning(move |number| {
            requests_for_mock.fetch_max(number, Ordering::SeqCst);
            Ok(payload(number))
        });
        let cancellation = CancellationToken::new();
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        let prefetcher =
            PayloadPrefetcher::new(Arc::new(source), cancellation.clone(), blocks_to_insert_tx);
        let handle = tokio::spawn(async move { prefetcher.run(0).await });

        let deadline = Instant::now() + Duration::from_secs(1);
        while blocks_to_insert_rx.len() < PREFETCH_WINDOW && Instant::now() < deadline {
            time::sleep(Duration::from_millis(10)).await;
        }
        let fetched = blocks_to_insert_rx.len();
        cancellation.cancel();
        drop(blocks_to_insert_rx);
        handle.await.expect("join").expect("prefetcher");

        assert_eq!(fetched, PREFETCH_WINDOW);
        assert!(requests.load(Ordering::SeqCst) <= PREFETCH_WINDOW as u64 + 1);
    }

    #[tokio::test]
    async fn proof_cap_pauses_until_proofs_advance() {
        let proofs_latest = Arc::new(AtomicU64::new(0));
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Number(number) => block_info(number),
                _ => block_info(0),
            }))
        });
        local.expect_l1_block_hash().returning(|_| Ok(Some(B256::ZERO)));
        let proofs_for_mock = Arc::clone(&proofs_latest);
        local
            .expect_proofs_latest()
            .returning(move || Ok(Some(proofs_for_mock.load(Ordering::SeqCst))));
        let local = Arc::new(local);

        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(20));
        source.expect_get_block_info().returning(|tag| {
            Ok(match tag {
                BlockNumberOrTag::Number(number) => source_block_info(number),
                _ => source_block_info(0),
            })
        });
        source.expect_get_payload_by_number().returning(|number| Ok(payload(number)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let cancellation = CancellationToken::new();
        let proof_gate = ActiveProofGate::new(Arc::clone(&local), DEFAULT_PROOFS_MAX_BLOCKS_AHEAD)
            .await
            .expect("proof gate");
        let engine_for_runtime: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let runtime = FollowRuntime::new(
            Arc::clone(&local),
            Arc::new(source),
            engine_for_runtime,
            cancellation.clone(),
            block_info(0),
            proof_gate,
            Duration::ZERO,
        );
        let handle = tokio::spawn(async move { runtime.start().await });

        time::sleep(Duration::from_millis(500)).await;
        assert_eq!(engine.inserted.lock().await.len(), DEFAULT_PROOFS_MAX_BLOCKS_AHEAD as usize);

        proofs_latest.store(10, Ordering::SeqCst);
        time::sleep(Duration::from_millis(500)).await;
        cancellation.cancel();
        handle.await.expect("join").expect("insert loop");

        assert!(engine.inserted.lock().await.len() > DEFAULT_PROOFS_MAX_BLOCKS_AHEAD as usize);
    }

    #[tokio::test]
    async fn safe_promotes_in_range_and_finalized_does_not_unwind() {
        // local: latest=10, safe=8, finalized=7. Source safe=9 (in range, coherent with local);
        // source finalized=6 (below local finalized, must not unwind).
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(9)));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            local,
            Arc::new(source),
            engine_for_update,
        )
        .await
        .expect("labels");

        assert_eq!(*engine.labels.lock().await, vec![(Some(9), None)]);
    }

    #[tokio::test]
    async fn safe_skips_when_source_ahead_of_local_latest() {
        // Source safe (20) is ahead of local latest (10), so block 20 is not locally verifiable.
        // Finalized (6) is below local finalized (7) and is skipped.
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(20)));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            local,
            Arc::new(source),
            engine_for_update,
        )
        .await
        .expect("labels");

        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn out_of_range_source_labels_skip_l1_origin_validation() {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest => block_info(10),
                BlockNumberOrTag::Safe => block_info(8),
                BlockNumberOrTag::Finalized => block_info(7),
                _ => panic!("unexpected local block lookup: {tag:?}"),
            }))
        });
        local.expect_l1_block_hash().times(0);

        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(20)));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            Arc::new(local),
            Arc::new(source),
            engine_for_update,
        )
        .await
        .expect("labels");

        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn safe_and_finalized_update_skips_without_local_safe_label() {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(match tag {
                BlockNumberOrTag::Latest => Some(block_info(10)),
                BlockNumberOrTag::Safe => None,
                _ => panic!("unexpected local block lookup: {tag:?}"),
            })
        });
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            Arc::new(local),
            Arc::new(MockRemoteClient::new()),
            engine_for_update,
        )
        .await
        .expect("skip update");

        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn safe_label_requests_recovery_on_source_hash_mismatch() {
        // Safe label hash disagrees with the local block at that height, so label promotion pauses
        // and signals recovery.
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Safe)).times(1).returning(|_| {
            Ok(BlockInfo { number: 10, hash: B256::from([99; 32]), ..Default::default() })
        });
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        let update =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
            )
            .await
            .expect("safe divergence recovery request");

        assert_eq!(update, SafetyUpdate::Recover);
        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn l2_origin_mismatch_does_not_block_label_promotion() {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest => block_info(10),
                BlockNumberOrTag::Safe => block_info(8),
                BlockNumberOrTag::Finalized => block_info(7),
                BlockNumberOrTag::Number(9) => L2BlockInfo {
                    l1_origin: alloy_eips::BlockNumHash {
                        number: 0,
                        hash: B256::with_last_byte(1),
                    },
                    ..block_info(9)
                },
                _ => panic!("unexpected local block lookup: {tag:?}"),
            }))
        });
        local.expect_l1_block_hash().returning(|_| Ok(Some(B256::ZERO)));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(9)));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            Arc::new(local),
            Arc::new(source),
            engine_for_update,
        )
        .await
        .expect("label update");

        assert_eq!(*engine.labels.lock().await, vec![(Some(9), None)]);
    }

    #[tokio::test]
    async fn unavailable_l2_origin_does_not_block_label_promotion() {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest => block_info(10),
                BlockNumberOrTag::Safe => block_info(8),
                BlockNumberOrTag::Finalized => block_info(7),
                BlockNumberOrTag::Number(9) => block_info(9),
                _ => panic!("unexpected local block lookup: {tag:?}"),
            }))
        });
        local.expect_l1_block_hash().returning(|_| Ok(None));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(9)));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .returning(|_| Ok(source_block_info(6)));
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            Arc::new(local),
            Arc::new(source),
            engine_for_update,
        )
        .await
        .expect("label update");

        assert_eq!(*engine.labels.lock().await, vec![(Some(9), None)]);
    }

    #[tokio::test]
    async fn finalized_label_rejects_source_hash_mismatch() {
        // The in-range finalized hash disagrees with local and reports a dedicated non-recoverable
        // divergence.
        let local = Arc::new(local_client(10, 9, 7, 100));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(9)));
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Finalized)).times(1).returning(
            |_| Ok(BlockInfo { number: 8, hash: B256::from([99; 32]), ..Default::default() }),
        );
        let mut engine = MockFollowEngine::new();
        engine.expect_insert_payload().times(0);
        engine.expect_update_safe_finalized_blocks().times(0);
        engine.expect_request_forkchoice().times(0);

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                Arc::new(engine),
            )
            .await
            .expect_err("mismatched finalized hash");

        assert!(matches!(error, FollowError::FinalizedDivergence { number: 8, .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn safe_divergence_pauses_and_resumes_after_fcu_sync() {
        let target = BlockInfo {
            number: 10,
            hash: B256::from([99; 32]),
            parent_hash: source_block_info(9).hash,
            ..Default::default()
        };
        let recovered = L2BlockInfo { block_info: target, ..Default::default() };
        let finalized = block_info(7);

        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Number(10) => block_info(10),
                BlockNumberOrTag::Safe | BlockNumberOrTag::Number(8) => block_info(8),
                BlockNumberOrTag::Finalized | BlockNumberOrTag::Number(7) => block_info(7),
                BlockNumberOrTag::Number(9) => block_info(9),
                _ => panic!("unexpected local block lookup: {tag:?}"),
            }))
        });
        local
            .expect_block_info_by_hash()
            .with(eq(target.hash))
            .once()
            .return_once(move |_| Ok(Some(recovered)));
        local.expect_l1_block_hash().returning(|_| Ok(Some(B256::ZERO)));

        let mut source = MockRemoteClient::new();
        let source_safe_calls = Arc::new(AtomicU64::new(0));
        let source_safe_calls_for_mock = Arc::clone(&source_safe_calls);
        // Initial label check matches, safety-tick detection diverges, recovery re-reads Safe.
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Safe)).times(3).returning(
            move |_| {
                if source_safe_calls_for_mock.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(source_block_info(8))
                } else {
                    Ok(target)
                }
            },
        );
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Finalized))
            .times(2)
            .returning(|_| Ok(source_block_info(7)));
        source.expect_get_block_info_by_hash().returning(|hash| {
            let number = u64::from(hash.as_slice()[31]);
            Ok(source_block_info(number))
        });
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(11));
        source.expect_get_payload_by_number().with(eq(11)).times(2).returning(|_| Ok(payload(11)));

        let engine = Arc::new(RecoveryRecordingEngine {
            inserted: Mutex::new(Vec::new()),
            forkchoices: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            head_requests: AtomicU64::new(0),
        });
        let cancellation = CancellationToken::new();
        let engine_for_runtime: Arc<dyn FollowEngine> =
            Arc::<RecoveryRecordingEngine>::clone(&engine);
        let runtime = FollowRuntime::new(
            Arc::new(local),
            Arc::new(source),
            engine_for_runtime,
            cancellation.clone(),
            block_info(10),
            PauseFirstGeneration { first_wait: true },
            Duration::ZERO,
        );
        let handle = tokio::spawn(async move { runtime.start().await });

        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(engine.inserted.lock().await.is_empty());

        time::advance(SAFETY_POLL_INTERVAL).await;
        for _ in 0..100 {
            if engine.head_requests.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(engine.head_requests.load(Ordering::SeqCst), 1);
        assert!(engine.inserted.lock().await.is_empty());

        time::advance(Duration::from_secs(1)).await;
        for _ in 0..100 {
            if !engine.inserted.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        handle.await.expect("join").expect("runtime");

        let forkchoices = engine.forkchoices.lock().await.clone();
        assert_eq!(forkchoices.len(), 2);
        for (head, safe, fcu_finalized) in &forkchoices {
            assert_eq!(*head, target);
            assert_eq!(*safe, finalized.block_info);
            assert_eq!(*fcu_finalized, finalized.block_info);
        }
        assert_eq!(engine.head_requests.load(Ordering::SeqCst), 2);
        assert_eq!(engine.labels.lock().await.last(), Some(&(Some(10), None)));
        assert_eq!(*engine.inserted.lock().await, vec![11]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_loop_benchmark_prefetches_source_fetch_latency() {
        let local = Arc::new(local_client(0, 0, 0, 100));
        let source = DelayedSource { latest: 25, fetch_delay: Duration::from_millis(30) };
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::from_millis(50),
        });
        let cancellation = CancellationToken::new();
        let engine_for_runtime: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let runtime = FollowRuntime::new(
            local,
            Arc::new(source),
            engine_for_runtime,
            cancellation.clone(),
            block_info(0),
            NoopProofGate,
            Duration::ZERO,
        );
        let started = Instant::now();
        let handle = tokio::spawn(async move { runtime.start().await });

        loop {
            if engine.inserted.lock().await.len() >= 20 {
                cancellation.cancel();
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        handle.await.expect("join").expect("insert loop");

        let elapsed_per_block = started.elapsed() / 20;
        assert!(
            elapsed_per_block < Duration::from_millis(75),
            "fetch latency appears serialized into insertion: {elapsed_per_block:?}"
        );
    }
}
