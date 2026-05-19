use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use base_protocol::{BlockInfo, L2BlockInfo};
use tokio::{
    sync::mpsc,
    time::{self, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::follow::{
    engine::FollowEngine,
    error::FollowError,
    local::FollowLocalClient,
    prefetcher::{PREFETCH_WINDOW, PayloadPrefetcher, PrefetchedPayload},
    proof_gate::ProofGate,
    source::RemoteClient,
};

const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);

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

    async fn run_update_safe_finalized_heads_loop(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
        cancellation: CancellationToken,
    ) -> Result<(), FollowError> {
        let mut ticker = time::interval(SAFETY_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }

            ticker.tick().await;
            if let Err(e) = Self::update_safe_and_finalized(
                Arc::clone(&local),
                Arc::clone(&source),
                Arc::clone(&engine),
            )
            .await
            {
                warn!(target: "follow", error = %e, "Failed to update safe/finalized labels");
            }
        }
    }

    async fn update_safe_and_finalized(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
    ) -> Result<(), FollowError> {
        let latest = local
            .block_info(BlockNumberOrTag::Latest)
            .await?
            .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Latest))?;
        let Some(local_safe) = local.block_info(BlockNumberOrTag::Safe).await? else {
            debug!(target: "follow", "Skipping safe/finalized update because local safe label is unavailable");
            return Ok(());
        };
        let local_finalized = local.block_info(BlockNumberOrTag::Finalized).await?;

        let source_safe =
            source.get_block_number(BlockNumberOrTag::Safe).await?.min(latest.block_info.number);
        let safe = if source_safe >= local_safe.block_info.number {
            Self::verified_local_block_at(&local, &source, source_safe).await?
        } else {
            None
        };

        let safe_limit = safe.as_ref().unwrap_or(&local_safe).block_info.number;
        let source_finalized = source
            .get_block_number(BlockNumberOrTag::Finalized)
            .await?
            .min(latest.block_info.number)
            .min(safe_limit);
        let local_finalized_number = local_finalized.map(|block| block.block_info.number);
        let should_update_finalized =
            local_finalized_number.map(|number| source_finalized >= number).unwrap_or(true);
        let finalized = if should_update_finalized {
            Self::verified_local_block_at(&local, &source, source_finalized).await?
        } else {
            None
        };

        engine.update_safe_finalized_blocks(safe, finalized).await
    }

    async fn verified_local_block_at(
        local: &Local,
        source: &Remote,
        number: u64,
    ) -> Result<Option<L2BlockInfo>, FollowError> {
        let Some(local_block) = local.block_info(number.into()).await? else {
            return Ok(None);
        };
        let source_block = source.get_block_info(number.into()).await?;
        Self::ensure_same_hash(local_block, source_block)?;
        Ok(Some(local_block))
    }

    fn ensure_same_hash(
        local_block: L2BlockInfo,
        source_block: BlockInfo,
    ) -> Result<(), FollowError> {
        if local_block.block_info.hash != source_block.hash {
            return Err(FollowError::SourceBlockHashMismatch {
                number: local_block.block_info.number,
                local: local_block.block_info.hash,
                remote: source_block.hash,
            });
        }
        Ok(())
    }
}

impl<Local, Remote, Gate> FollowRuntime<Local, Remote, Gate>
where
    Local: FollowLocalClient + 'static,
    Remote: RemoteClient + 'static,
    Gate: ProofGate + 'static,
{
    pub(super) async fn start(mut self) -> Result<(), FollowError> {
        let next_insert = self.follow_from_block.block_info.number.saturating_add(1);
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        let prefetcher = PayloadPrefetcher::new(
            Arc::clone(&self.source),
            self.cancellation.clone(),
            blocks_to_insert_tx,
        );
        let fetch_loop = prefetcher.run(self.follow_from_block.block_info.number);
        let insert_loop = Self::run_ordered_insert_loop(
            Arc::clone(&self.engine),
            self.cancellation.clone(),
            blocks_to_insert_rx,
            next_insert,
            &mut self.proof_gate,
            self.insert_delay,
        );
        let safety_loop = Self::run_update_safe_finalized_heads_loop(
            Arc::clone(&self.local),
            Arc::clone(&self.source),
            Arc::clone(&self.engine),
            self.cancellation.clone(),
        );

        tokio::select! {
            result = fetch_loop => result,
            result = insert_loop => result,
            result = safety_loop => result,
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
    use base_protocol::L2BlockInfo;
    use mockall::predicate::eq;
    use tokio::{sync::Mutex, time};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        MockRemoteClient,
        follow::{
            engine::FollowEngine,
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
            self.labels
                .lock()
                .await
                .push((safe.map(|v| v.block_info.number), finalized.map(|v| v.block_info.number)));
            Ok(())
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
        BlockInfo { number, hash: B256::from([number as u8; 32]), ..Default::default() }
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
        let proofs_for_mock = Arc::clone(&proofs_latest);
        local
            .expect_proofs_latest()
            .returning(move || Ok(Some(proofs_for_mock.load(Ordering::SeqCst))));
        let local = Arc::new(local);

        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(20));
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(0));
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Finalized)).returning(|_| Ok(0));
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
    async fn safe_and_finalized_are_clamped_and_do_not_unwind() {
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(20));
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Finalized)).returning(|_| Ok(6));
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Number(10)))
            .returning(|_| Ok(source_block_info(10)));
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

        assert_eq!(*engine.labels.lock().await, vec![(Some(10), None)]);
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
    async fn safe_label_rejects_source_hash_mismatch() {
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Safe)).returning(|_| Ok(10));
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Number(10))).returning(|_| {
            Ok(BlockInfo { number: 10, hash: B256::from([99; 32]), ..Default::default() })
        });
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
            )
            .await
            .expect_err("mismatched source hash");

        assert!(matches!(error, FollowError::SourceBlockHashMismatch { number: 10, .. }));
        assert!(engine.labels.lock().await.is_empty());
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
