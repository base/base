use std::{fmt::Debug, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
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
    recovery,
    source::RemoteClient,
};

const SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Outcome of the safety loop / a single safe-finalized update pass.
#[derive(Debug, PartialEq, Eq)]
enum SafetyOutcome {
    /// Safe/finalized labels were updated, intentionally skipped, or the loop was cancelled.
    Updated,
    /// The local chain diverged from the source and was reset to the common ancestor; fetch/insert
    /// must restart from the recovery plan's ancestor to replay the captured source branch.
    Reorged {
        /// Common ancestor and source branch to replay.
        plan: recovery::RecoveryPlan,
    },
}

/// Result of evaluating one coherent source label block against the local chain at that height.
enum LabelOutcome {
    /// Promote the local block as the new label.
    Promote(L2BlockInfo),
    /// Skip promotion this round (label out of range, or local block missing).
    Skip,
    /// The local block at the label's height has a different hash than the source.
    Diverged {
        /// Block number that diverged.
        number: u64,
        /// Hash on the local node.
        local: B256,
        /// Hash on the source node.
        remote: B256,
    },
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

            let expected_hash = payload.execution_payload.block_hash();
            info!(target: "follow", block = current_block, "Inserting source payload");
            let inserted = engine.insert_payload(payload).await?;
            if inserted.block_info.number != current_block
                || inserted.block_info.hash != expected_hash
            {
                return Err(FollowError::PayloadNotApplied {
                    expected_number: current_block,
                    expected_hash,
                    actual_number: inserted.block_info.number,
                    actual_hash: inserted.block_info.hash,
                });
            }
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
        generation: CancellationToken,
        cancellation: CancellationToken,
    ) -> Result<SafetyOutcome, FollowError> {
        let mut ticker = time::interval(SAFETY_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if cancellation.is_cancelled() {
                return Ok(SafetyOutcome::Updated);
            }

            ticker.tick().await;
            match Self::update_safe_and_finalized(
                Arc::clone(&local),
                Arc::clone(&source),
                Arc::clone(&engine),
                generation.clone(),
            )
            .await
            {
                Ok(SafetyOutcome::Updated) => {}
                // A recovery reorg ends the loop so the caller restarts fetch/insert from the
                // common ancestor.
                Ok(outcome @ SafetyOutcome::Reorged { .. }) => return Ok(outcome),
                Err(FollowError::FinalizedDivergence { number, local, remote }) => {
                    error!(
                        target: "follow",
                        number,
                        local = %local,
                        source = %remote,
                        "Local finalized head diverged from source; follow mode requires operator intervention",
                    );
                }
                Err(error) => {
                    warn!(target: "follow", error = %error, "Failed to update safe/finalized labels");
                }
            }
        }
    }

    async fn update_safe_and_finalized(
        local: Arc<Local>,
        source: Arc<Remote>,
        engine: Arc<dyn FollowEngine>,
        generation: CancellationToken,
    ) -> Result<SafetyOutcome, FollowError> {
        let latest = local
            .block_info(BlockNumberOrTag::Latest)
            .await?
            .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Latest))?;
        let Some(local_safe) = local.block_info(BlockNumberOrTag::Safe).await? else {
            debug!(target: "follow", "Skipping safe/finalized update because local safe label is unavailable");
            return Ok(SafetyOutcome::Updated);
        };
        let local_finalized = local.block_info(BlockNumberOrTag::Finalized).await?;

        // Coherent label read: number and hash from one response (mirrors op-node
        // `L2BlockRefByLabel`).
        let source_safe = source.get_block_info(BlockNumberOrTag::Safe).await?;
        let safe = match Self::evaluate_label(
            &local,
            &source_safe,
            local_safe.block_info.number,
            latest.block_info.number,
        )
        .await?
        {
            LabelOutcome::Promote(block) => {
                Self::validate_source_l1_origin(local.as_ref(), &block).await?;
                Some(block)
            }
            LabelOutcome::Skip => None,
            LabelOutcome::Diverged { number, local: local_hash, remote } => {
                // Reset to the common ancestor and replay the source branch forward.
                warn!(
                    target: "follow",
                    number,
                    local = %local_hash,
                    source = %remote,
                    "Local chain diverged from source safe head; recovering to common ancestor",
                );
                let finalized = local_finalized
                    .as_ref()
                    .ok_or(FollowError::LocalBlockUnavailable(BlockNumberOrTag::Finalized))?;
                let plan = recovery::recover(
                    local.as_ref(),
                    source.as_ref(),
                    &engine,
                    finalized,
                    &source_safe,
                    local_hash,
                    generation.clone(),
                )
                .await?;
                if generation.is_cancelled() {
                    return Ok(SafetyOutcome::Updated);
                }
                // Cancel fetch/insert after the reset so this generation can return the restart
                // point.
                generation.cancel();
                return Ok(SafetyOutcome::Reorged { plan });
            }
        };

        let safe_limit = safe.as_ref().unwrap_or(&local_safe).block_info.number;

        // Finalized floor at the local finalized head (never unwind), ceiling at the safe head.
        let source_finalized = source.get_block_info(BlockNumberOrTag::Finalized).await?;
        let local_finalized_number =
            local_finalized.as_ref().map(|block| block.block_info.number).unwrap_or_default();
        let finalized = match Self::evaluate_label(
            &local,
            &source_finalized,
            local_finalized_number,
            latest.block_info.number.min(safe_limit),
        )
        .await?
        {
            LabelOutcome::Promote(block) => {
                Self::validate_source_l1_origin(local.as_ref(), &block).await?;
                Some(block)
            }
            LabelOutcome::Skip => None,
            LabelOutcome::Diverged { number, local: local_hash, remote } => {
                return Err(FollowError::FinalizedDivergence { number, local: local_hash, remote });
            }
        };

        engine.update_safe_finalized_blocks(safe, finalized).await?;
        Ok(SafetyOutcome::Updated)
    }

    /// Evaluates a coherent source label block (`{number, hash}` from a single read) against the
    /// local chain at the same height. Returns [`LabelOutcome::Skip`] when the label is out of
    /// `[floor, ceiling]` or the local block is missing, [`LabelOutcome::Promote`] when the hashes
    /// match, and [`LabelOutcome::Diverged`] when the local block at that height differs.
    async fn evaluate_label(
        local: &Local,
        source_block: &BlockInfo,
        floor: u64,
        ceiling: u64,
    ) -> Result<LabelOutcome, FollowError> {
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
            return Ok(LabelOutcome::Skip);
        }
        let Some(local_block) = local.block_info(number.into()).await? else {
            return Ok(LabelOutcome::Skip);
        };
        if local_block.block_info.hash != source_block.hash {
            return Ok(LabelOutcome::Diverged {
                number,
                local: local_block.block_info.hash,
                remote: source_block.hash,
            });
        }
        Ok(LabelOutcome::Promote(local_block))
    }

    /// Verifies that a hash-verified local block's L1 origin is canonical in the local L1 view.
    async fn validate_source_l1_origin(
        local: &Local,
        block: &L2BlockInfo,
    ) -> Result<(), FollowError> {
        let origin = block.l1_origin;
        let local_hash = local
            .l1_block_hash(origin.number)
            .await?
            .ok_or(FollowError::LocalL1BlockUnavailable(origin.number))?;
        if local_hash != origin.hash {
            return Err(FollowError::SourceL1OriginMismatch {
                l2_number: block.block_info.number,
                l1_number: origin.number,
                local: local_hash,
                remote: origin.hash,
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
        let mut head_number = self.follow_from_block.block_info.number;
        let mut replay = Vec::new();

        loop {
            if self.cancellation.is_cancelled() {
                return Ok(());
            }

            let next_insert = head_number.saturating_add(1);

            // Fetch + insert share a child token so a recovery reorg can restart just those loops
            // from the new head while the parent cancellation still drives full shutdown.
            let generation = self.cancellation.child_token();
            let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
            let prefetcher = PayloadPrefetcher::new(
                Arc::clone(&self.source),
                generation.clone(),
                blocks_to_insert_tx,
            );
            // Pin the safety future so an insert stall can await the same in-flight reconciliation
            // after catch-up futures are dropped.
            let mut safety_loop = Box::pin(Self::run_update_safe_finalized_heads_loop(
                Arc::clone(&self.local),
                Arc::clone(&self.source),
                Arc::clone(&self.engine),
                generation.clone(),
                self.cancellation.clone(),
            ));

            let selected = {
                let fetch_loop = prefetcher.run(head_number, replay);
                let insert_loop = Self::run_ordered_insert_loop(
                    Arc::clone(&self.engine),
                    generation.clone(),
                    blocks_to_insert_rx,
                    next_insert,
                    &mut self.proof_gate,
                    self.insert_delay,
                );
                tokio::pin!(fetch_loop);
                tokio::pin!(insert_loop);

                tokio::select! {
                    result = &mut fetch_loop => GenerationSelect::Finished(
                        result.map(|()| SafetyOutcome::Updated),
                    ),
                    result = &mut insert_loop => match result {
                        Ok(()) => GenerationSelect::Finished(Ok(SafetyOutcome::Updated)),
                        Err(FollowError::PayloadNotApplied {
                            expected_number,
                            expected_hash,
                            actual_number,
                            actual_hash,
                        }) => GenerationSelect::InsertionStalled {
                            expected_number,
                            expected_hash,
                            actual_number,
                            actual_hash,
                        },
                        Err(error) => GenerationSelect::Finished(Err(error)),
                    },
                    result = &mut safety_loop => GenerationSelect::Finished(result),
                }
            };

            let outcome = match selected {
                GenerationSelect::Finished(result) => result?,
                GenerationSelect::InsertionStalled {
                    expected_number,
                    expected_hash,
                    actual_number,
                    actual_hash,
                } => {
                    // Pause catch-up and finish reconciliation on the in-flight safety loop.
                    warn!(
                        target: "follow",
                        expected_number,
                        expected_hash = %expected_hash,
                        actual_number,
                        actual_hash = %actual_hash,
                        "Source payload was not applied; pausing insertion until safety reconciliation completes",
                    );
                    safety_loop.await?
                }
            };

            match outcome {
                SafetyOutcome::Reorged { plan } => {
                    // The engine reset the unsafe head down to the common ancestor (a block it
                    // has, so the forkchoice update was Valid). Restart from there and replay the
                    // captured source branch by hash.
                    let ancestor = plan.ancestor.block_info.number;
                    info!(
                        target: "follow",
                        ancestor,
                        "Reset to common ancestor; restarting follow fetch/insert",
                    );
                    head_number = ancestor;
                    replay = plan.replay;
                }
                SafetyOutcome::Updated => return Ok(()),
            }
        }
    }
}

/// Result of selecting among fetch, insert, and safety work for one follow generation.
enum GenerationSelect {
    /// Fetch, insert, or safety finished with a definitive generation outcome.
    Finished(Result<SafetyOutcome, FollowError>),
    /// Insert could not apply a source payload; await the in-flight safety loop for reconciliation.
    InsertionStalled {
        /// Source payload block number that was not applied.
        expected_number: u64,
        /// Source payload block hash that was not applied.
        expected_hash: B256,
        /// Engine head block number after the rejected insert.
        actual_number: u64,
        /// Engine head block hash after the rejected insert.
        actual_hash: B256,
    },
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
            engine::{FollowEngine, ResetStats},
            local::MockFollowLocalClient,
            proof_gate::{ActiveProofGate, NoopProofGate},
        },
    };

    const DEFAULT_PROOFS_MAX_BLOCKS_AHEAD: u64 = 16;

    #[derive(Debug)]
    struct RecordingEngine {
        inserted: Mutex<Vec<u64>>,
        labels: Mutex<Vec<(Option<u64>, Option<u64>)>>,
        reset: Mutex<Vec<u64>>,
        delay: Duration,
        advance: bool,
        /// When set, inserts report a non-advancing head until [`FollowEngine::reset_to_ancestor`].
        stall_until_reset: bool,
    }

    impl RecordingEngine {
        fn new(delay: Duration) -> Self {
            Self {
                inserted: Mutex::new(Vec::new()),
                labels: Mutex::new(Vec::new()),
                reset: Mutex::new(Vec::new()),
                delay,
                advance: true,
                stall_until_reset: false,
            }
        }

        fn non_advancing(delay: Duration) -> Self {
            Self { advance: false, ..Self::new(delay) }
        }

        fn stall_inserts_until_reset(delay: Duration) -> Self {
            Self { stall_until_reset: true, ..Self::new(delay) }
        }
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

        async fn get_block_info_by_hash(
            &self,
            hash: B256,
        ) -> Result<BlockInfo, crate::RemoteL2ClientError> {
            Ok(source_block_info(hash[31] as u64))
        }

        async fn get_payload_by_number(
            &self,
            number: u64,
        ) -> Result<BaseExecutionPayloadEnvelope, crate::RemoteL2ClientError> {
            time::sleep(self.fetch_delay).await;
            Ok(payload(number))
        }

        async fn get_payload_by_hash(
            &self,
            hash: B256,
        ) -> Result<BaseExecutionPayloadEnvelope, crate::RemoteL2ClientError> {
            time::sleep(self.fetch_delay).await;
            Ok(payload(hash[31] as u64))
        }
    }

    #[async_trait]
    impl FollowEngine for RecordingEngine {
        async fn insert_payload(
            &self,
            envelope: BaseExecutionPayloadEnvelope,
        ) -> Result<L2BlockInfo, FollowError> {
            time::sleep(self.delay).await;
            let number = envelope.execution_payload.block_number();
            let hash = envelope.execution_payload.block_hash();
            self.inserted.lock().await.push(number);
            let advance =
                self.advance && (!self.stall_until_reset || !self.reset.lock().await.is_empty());
            if advance {
                return Ok(L2BlockInfo {
                    block_info: BlockInfo { number, hash, ..Default::default() },
                    ..Default::default()
                });
            }
            Ok(block_info(number.saturating_sub(1)))
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

        async fn reset_to_ancestor(
            &self,
            ancestor: L2BlockInfo,
            _cancellation: CancellationToken,
        ) -> Result<ResetStats, FollowError> {
            self.reset.lock().await.push(ancestor.block_info.number);
            Ok(ResetStats::default())
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
            parent_hash: B256::from([number.saturating_sub(1) as u8; 32]),
            ..Default::default()
        }
    }

    fn fork_source_block_info(number: u64, offset: u8) -> BlockInfo {
        BlockInfo {
            number,
            hash: B256::from([number as u8 + offset; 32]),
            parent_hash: B256::from([number.saturating_sub(1) as u8 + offset; 32]),
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
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
    async fn ordered_insertion_rejects_engine_noop_without_advancing_cursor() {
        let engine = Arc::new(RecordingEngine::non_advancing(Duration::ZERO));
        let mut proof_gate = NoopProofGate;
        let (blocks_to_insert_tx, blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        blocks_to_insert_tx.send(payload(1)).await.expect("send 1");
        drop(blocks_to_insert_tx);

        let engine_for_loop: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::run_ordered_insert_loop(
                engine_for_loop,
                CancellationToken::new(),
                blocks_to_insert_rx,
                1,
                &mut proof_gate,
                Duration::ZERO,
            )
            .await
            .expect_err("non-advancing insert must fail");

        assert!(matches!(
            error,
            FollowError::PayloadNotApplied { expected_number: 1, actual_number: 0, .. }
        ));
        assert_eq!(*engine.inserted.lock().await, vec![1]);
    }

    /// Source whose safe label is delayed and disagrees with the local tip at that height.
    #[derive(Debug)]
    struct DivergentRestartSource {
        safe_delay: Duration,
    }

    #[async_trait]
    impl RemoteClient for DivergentRestartSource {
        async fn get_block_number(
            &self,
            tag: BlockNumberOrTag,
        ) -> Result<u64, crate::RemoteL2ClientError> {
            match tag {
                BlockNumberOrTag::Latest => Ok(2),
                BlockNumberOrTag::Number(number) => Ok(number),
                BlockNumberOrTag::Safe => Ok(1),
                _ => Ok(0),
            }
        }

        async fn get_block_info(
            &self,
            tag: BlockNumberOrTag,
        ) -> Result<BlockInfo, crate::RemoteL2ClientError> {
            match tag {
                BlockNumberOrTag::Safe => {
                    time::sleep(self.safe_delay).await;
                    Ok(BlockInfo {
                        number: 1,
                        hash: B256::from([99; 32]),
                        parent_hash: B256::from([0; 32]),
                        ..Default::default()
                    })
                }
                BlockNumberOrTag::Latest => Ok(BlockInfo {
                    number: 2,
                    hash: B256::from([2; 32]),
                    parent_hash: B256::from([99; 32]),
                    ..Default::default()
                }),
                BlockNumberOrTag::Number(number) => Ok(source_block_info(number)),
                _ => Ok(source_block_info(0)),
            }
        }

        async fn get_block_info_by_hash(
            &self,
            hash: B256,
        ) -> Result<BlockInfo, crate::RemoteL2ClientError> {
            if hash == B256::from([99; 32]) {
                return Ok(BlockInfo {
                    number: 1,
                    hash,
                    parent_hash: B256::from([0; 32]),
                    ..Default::default()
                });
            }
            if hash == B256::from([2; 32]) {
                return Ok(BlockInfo {
                    number: 2,
                    hash,
                    parent_hash: B256::from([99; 32]),
                    ..Default::default()
                });
            }
            Ok(source_block_info(hash[31] as u64))
        }

        async fn get_payload_by_number(
            &self,
            number: u64,
        ) -> Result<BaseExecutionPayloadEnvelope, crate::RemoteL2ClientError> {
            Ok(payload(number))
        }

        async fn get_payload_by_hash(
            &self,
            hash: B256,
        ) -> Result<BaseExecutionPayloadEnvelope, crate::RemoteL2ClientError> {
            let mut envelope = if hash == B256::from([99; 32]) {
                payload(1)
            } else if hash == B256::from([2; 32]) {
                payload(2)
            } else {
                payload(hash[31] as u64)
            };
            if let BaseExecutionPayload::V1(payload) = &mut envelope.execution_payload {
                payload.block_hash = hash;
                payload.parent_hash = if hash == B256::from([2; 32]) {
                    B256::from([99; 32])
                } else if hash == B256::from([99; 32]) {
                    B256::from([0; 32])
                } else {
                    B256::from([hash[31].saturating_sub(1); 32])
                };
            }
            Ok(envelope)
        }
    }

    #[tokio::test]
    async fn payload_not_applied_awaits_safety_recovery() {
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Latest => block_info(1),
                BlockNumberOrTag::Number(number) => block_info(number),
                _ => block_info(0),
            }))
        });
        local.expect_proofs_latest().returning(|| Ok(Some(100)));

        let engine = Arc::new(RecordingEngine::stall_inserts_until_reset(Duration::ZERO));
        let cancellation = CancellationToken::new();
        let engine_for_runtime: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let runtime = FollowRuntime::new(
            Arc::new(local),
            Arc::new(DivergentRestartSource { safe_delay: Duration::from_millis(50) }),
            engine_for_runtime,
            cancellation.clone(),
            block_info(1),
            NoopProofGate,
            Duration::ZERO,
        );
        let handle = tokio::spawn(async move { runtime.start().await });

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !engine.reset.lock().await.is_empty()
                && engine.inserted.lock().await.iter().any(|number| *number >= 2)
            {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(*engine.reset.lock().await, vec![0]);
        assert!(
            engine.inserted.lock().await.contains(&1),
            "recovery must replay the source branch after reset: {:?}",
            engine.inserted.lock().await
        );
        assert!(
            engine.inserted.lock().await.iter().any(|number| *number >= 2),
            "follow must resume catch-up after recovery: {:?}",
            engine.inserted.lock().await
        );

        cancellation.cancel();
        handle.await.expect("join").expect("follow recovered after PayloadNotApplied");
    }

    #[tokio::test]
    async fn ordered_insertion_applies_configured_insert_delay() {
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
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
        let handle = tokio::spawn(async move { prefetcher.run(0, Vec::new()).await });

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
    async fn prefetch_replays_captured_branch_by_hash() {
        let hash = B256::from([1; 32]);
        let mut source = MockRemoteClient::new();
        source.expect_get_payload_by_hash().with(eq(hash)).times(1).returning(|_| Ok(payload(1)));
        source.expect_get_block_number().with(eq(BlockNumberOrTag::Latest)).returning(|_| Ok(1));

        let cancellation = CancellationToken::new();
        let (blocks_to_insert_tx, mut blocks_to_insert_rx) = mpsc::channel(PREFETCH_WINDOW);
        let prefetcher =
            PayloadPrefetcher::new(Arc::new(source), cancellation.clone(), blocks_to_insert_tx);
        let handle = tokio::spawn(async move {
            prefetcher
                .run(0, vec![recovery::ReplayBlock { number: 1, hash, parent_hash: B256::ZERO }])
                .await
        });

        let replayed = time::timeout(Duration::from_secs(1), blocks_to_insert_rx.recv())
            .await
            .expect("replay payload timeout")
            .expect("replay payload");
        assert_eq!(replayed.execution_payload.block_hash(), hash);

        cancellation.cancel();
        drop(blocks_to_insert_rx);
        handle.await.expect("join").expect("prefetcher");
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            local,
            Arc::new(source),
            engine_for_update,
            CancellationToken::new(),
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            local,
            Arc::new(source),
            engine_for_update,
            CancellationToken::new(),
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
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
            Arc::new(local),
            Arc::new(MockRemoteClient::new()),
            engine_for_update,
            CancellationToken::new(),
        )
        .await
        .expect("skip update");

        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn safe_divergence_recovers_to_ancestor() {
        // Safe label (number + hash) disagrees with the local block at height 10. Recovery resets
        // to the common ancestor. Source latest is on the same branch as source safe, so the plan
        // includes block 10 for hash-pinned replay.
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Safe)).returning(|_| {
            Ok(BlockInfo {
                number: 10,
                hash: B256::from([99; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            })
        });
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Latest)).returning(|_| {
            Ok(BlockInfo {
                number: 10,
                hash: B256::from([99; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            })
        });
        source
            .expect_get_block_info_by_hash()
            .with(eq(B256::from([9; 32])))
            .returning(|_| Ok(source_block_info(9)));
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let generation = CancellationToken::new();

        let outcome =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
                generation.clone(),
            )
            .await
            .expect("recover from divergence");

        let SafetyOutcome::Reorged { plan } = outcome else {
            panic!("expected recovery outcome");
        };
        assert_eq!(plan.ancestor.block_info.number, 9);
        assert_eq!(plan.replay.len(), 1);
        assert_eq!(plan.replay[0].number, 10);
        assert_eq!(plan.replay[0].hash, B256::from([99; 32]));
        assert!(generation.is_cancelled());
        assert_eq!(*engine.reset.lock().await, vec![9]);
        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn verified_local_l1_origin_must_be_canonical_locally() {
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
        let engine = Arc::new(RecordingEngine {
            inserted: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        });
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                Arc::new(local),
                Arc::new(source),
                engine_for_update,
                CancellationToken::new(),
            )
            .await
            .expect_err("verified local L1 origin must match local canonical L1");

        assert!(matches!(
            error,
            FollowError::SourceL1OriginMismatch { l2_number: 9, l1_number: 0, .. }
        ));
        assert!(engine.labels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn recovery_rejects_source_latest_from_different_branch() {
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Safe)).returning(|_| {
            Ok(BlockInfo {
                number: 10,
                hash: B256::from([99; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            })
        });
        source
            .expect_get_block_info_by_hash()
            .with(eq(B256::from([9; 32])))
            .returning(|_| Ok(source_block_info(9)));
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Latest)).returning(|_| {
            Ok(BlockInfo {
                number: 10,
                hash: B256::from([100; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            })
        });
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let generation = CancellationToken::new();

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
                generation.clone(),
            )
            .await
            .expect_err("incoherent source branch");

        assert!(matches!(error, FollowError::SourceBranchMismatch { number: 10, .. }));
        assert!(!generation.is_cancelled());
        assert!(engine.reset.lock().await.is_empty());
    }

    #[tokio::test]
    async fn failed_safe_recovery_keeps_fetch_generation_running() {
        let local = Arc::new(local_client(10, 8, 7, 100));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(fork_source_block_info(10, 100)));
        source.expect_get_block_info_by_hash().returning(|hash| {
            let number = hash[31].saturating_sub(100) as u64;
            Ok(fork_source_block_info(number, 100))
        });
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let generation = CancellationToken::new();

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
                generation.clone(),
            )
            .await
            .expect_err("source branch diverges at finalized");

        assert!(matches!(error, FollowError::FinalizedDivergence { number: 7, .. }));
        assert!(!generation.is_cancelled());
        assert!(engine.reset.lock().await.is_empty());
    }

    #[tokio::test]
    async fn finalized_label_rejects_source_hash_mismatch() {
        // Safe label is consistent so evaluation reaches finalized; the in-range finalized hash
        // disagrees with local, which returns FinalizedDivergence without resetting.
        let local = Arc::new(local_client(10, 9, 7, 100));
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info()
            .with(eq(BlockNumberOrTag::Safe))
            .returning(|_| Ok(source_block_info(9)));
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Finalized)).times(1).returning(
            |_| Ok(BlockInfo { number: 8, hash: B256::from([99; 32]), ..Default::default() }),
        );
        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_update: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);

        let error =
            FollowRuntime::<MockFollowLocalClient, MockRemoteClient, NoopProofGate>::update_safe_and_finalized(
                local,
                Arc::new(source),
                engine_for_update,
                CancellationToken::new(),
            )
            .await
            .expect_err("mismatched finalized hash");

        assert!(matches!(error, FollowError::FinalizedDivergence { number: 8, .. }));
        assert!(engine.labels.lock().await.is_empty());
        assert!(engine.reset.lock().await.is_empty());
    }

    #[tokio::test]
    async fn find_common_ancestor_follows_captured_source_branch() {
        // Local and source agree at and below block 5 and disagree above it. A divergence detected
        // at block 10 must converge to block 5 as the highest common ancestor.
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            let number = match tag {
                BlockNumberOrTag::Number(n) => n,
                BlockNumberOrTag::Finalized => 2,
                _ => 0,
            };
            // Above the agreement boundary the local chain is on a different fork.
            let hash = if number <= 5 {
                B256::from([number as u8; 32])
            } else {
                B256::from([number as u8 + 100; 32])
            };
            Ok(Some(L2BlockInfo {
                block_info: base_protocol::BlockInfo { number, hash, ..Default::default() },
                ..Default::default()
            }))
        });
        let mut source = MockRemoteClient::new();
        source
            .expect_get_block_info_by_hash()
            .returning(|hash| Ok(source_block_info(hash[31] as u64)));

        let finalized = block_info(2);
        let source_safe = source_block_info(10);
        let ancestor = recovery::find_common_ancestor(&local, &source, &finalized, &source_safe)
            .await
            .expect("common ancestor");

        assert_eq!(ancestor.block_info.number, 5);
    }

    #[tokio::test]
    async fn recover_rejects_finalized_hash_mismatch() {
        // Source disagrees with local at the finalized height. Recovery must not reset below
        // finalized, so this returns FinalizedDivergence.
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(|tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Finalized => block_info(7),
                BlockNumberOrTag::Number(number) => block_info(number),
                _ => block_info(0),
            }))
        });
        let mut source = MockRemoteClient::new();
        source.expect_get_block_info_by_hash().returning(|hash| {
            let number = hash[31].saturating_sub(100) as u64;
            Ok(BlockInfo {
                number,
                hash,
                parent_hash: B256::from([number.saturating_sub(1) as u8 + 100; 32]),
                ..Default::default()
            })
        });
        let engine: Arc<dyn FollowEngine> = Arc::new(RecordingEngine::new(Duration::ZERO));
        let finalized = block_info(7);
        let source_safe = BlockInfo {
            number: 10,
            hash: B256::from([110; 32]),
            parent_hash: B256::from([109; 32]),
            ..Default::default()
        };

        let error = recovery::recover(
            &local,
            &source,
            &engine,
            &finalized,
            &source_safe,
            B256::from([7; 32]),
            CancellationToken::new(),
        )
        .await
        .expect_err("finalized divergence");

        assert!(matches!(error, FollowError::FinalizedDivergence { number: 7, .. }));
    }

    #[tokio::test]
    async fn recovery_rechecks_fresh_finalized_head_before_reset() {
        let finalized_reads = Arc::new(AtomicU64::new(0));
        let finalized_reads_for_local = Arc::clone(&finalized_reads);
        let mut local = MockFollowLocalClient::new();
        local.expect_block_info().returning(move |tag| {
            Ok(Some(match tag {
                BlockNumberOrTag::Finalized => {
                    finalized_reads_for_local.fetch_add(1, Ordering::SeqCst);
                    block_info(10)
                }
                BlockNumberOrTag::Number(number) => block_info(number),
                _ => block_info(0),
            }))
        });

        let mut source = MockRemoteClient::new();
        source.expect_get_block_info().with(eq(BlockNumberOrTag::Latest)).returning(|_| {
            Ok(BlockInfo {
                number: 10,
                hash: B256::from([99; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            })
        });
        source
            .expect_get_block_info_by_hash()
            .with(eq(B256::from([9; 32])))
            .returning(|_| Ok(source_block_info(9)));

        let engine = Arc::new(RecordingEngine::new(Duration::ZERO));
        let engine_for_recovery: Arc<dyn FollowEngine> = Arc::<RecordingEngine>::clone(&engine);
        let error = recovery::recover(
            &local,
            &source,
            &engine_for_recovery,
            &block_info(7),
            &BlockInfo {
                number: 10,
                hash: B256::from([99; 32]),
                parent_hash: B256::from([9; 32]),
                ..Default::default()
            },
            B256::from([10; 32]),
            CancellationToken::new(),
        )
        .await
        .expect_err("fresh finalized divergence must prevent reset");

        assert!(matches!(error, FollowError::FinalizedDivergence { number: 10, .. }));
        assert_eq!(finalized_reads.load(Ordering::SeqCst), 1);
        assert!(engine.reset.lock().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_loop_benchmark_prefetches_source_fetch_latency() {
        let local = Arc::new(local_client(0, 0, 0, 100));
        let source = DelayedSource { latest: 25, fetch_delay: Duration::from_millis(30) };
        let engine = Arc::new(RecordingEngine::new(Duration::from_millis(50)));
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
