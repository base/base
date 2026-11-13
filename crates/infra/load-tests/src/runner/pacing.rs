//! Inventory headroom, presign streaming, and measured enqueue pacing.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::Eip8130Constants;
use base_common_network::Base;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::{
    sync::{mpsc, watch},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    BlockPulse, BlockWatcher, CanonicalHeadWatcher, GasEstimate, GasPricer, LoadRunner,
    LoadTestStage, PipelineStartConfig, PreparedTransaction, PresignBuffer, ProgressReport,
    RefillTrigger, ResultsTracker, RunCoordinator, SignedTransaction, SubmissionPipeline,
    SubmitCohort, TxType, ValidityRouter,
};
use crate::{
    BaselineError, Result,
    metrics::{MetricsSummary, PacingCycleObservation, PacingCycleSource},
    rpc::{BaseFeeExt, QueryProvider},
    workload::{KeyStream, SeededRng, SelectedPayload, WorkloadGenerator},
};

const SUBMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMIT_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const CONFIRMATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(200);
/// How long the post-run drain waits for a landing before it stops tracking the remaining
/// inventory, so stuck stragglers cannot hold a short test for the full drain timeout.
const DRAIN_STALL_TIMEOUT: Duration = Duration::from_secs(20);
const WATCHER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SIGNED_BATCH_SIZE: usize = 200;
const PRESIGN_CHANNEL_BUFFER: usize = 2;
/// Run coordinator inbox capacity. Every task blocks on a full inbox, which bounds the
/// coordinator's backlog instead of letting events pile up.
const RUN_EVENT_BUFFER: usize = 4096;
/// How long a refill cycle waits for its batches to be acknowledged before turning late.
const REFILL_ACK_BUDGET: Duration = Duration::from_millis(100);
/// Multiple of the target rate a steady-submission cycle may send over the time since the previous
/// cycle: late cycles still catch up, while a backlog left by a stall drains at twice the target
/// rate rather than in one burst.
const STEADY_CATCHUP_RATE: u128 = 2;
const PREFILL_TIMEOUT: Duration = Duration::from_secs(300);
const START_FILE_TIMEOUT: Duration = Duration::from_secs(300);
const START_FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct SenderJob {
    sender_index: usize,
    generation: u64,
    from: Address,
    start_nonce: u64,
    prepared_txs: Vec<PreparedTransaction>,
}

#[derive(Debug)]
struct PresignProducerState {
    generator: WorkloadGenerator,
    /// Fixed payload for the validity priority-lead cohort, built once per run. `None` keeps
    /// the lead cohort on the weighted mix.
    lead_payload: Option<SelectedPayload>,
    recipient_keys: Option<KeyStream>,
    recipient_rng: SeededRng,
}

/// Producer-owned nonce lane of one sender.
#[derive(Debug, Clone, Copy)]
struct ProducerSender {
    address: Address,
    next_nonce: u64,
    /// Bumped when submission reports a terminal rejection so stale signed chunks cannot be
    /// enqueued after a nonce resync.
    generation: u64,
}

struct PresignConfig {
    senders: Vec<ProducerSender>,
    /// Sender keys indexed like `senders`.
    signers: Arc<[PrivateKeySigner]>,
    chain_id: u64,
    base_fee_rx: watch::Receiver<u128>,
    max_gas_price: u128,
    validity_priority_lead_multiplier: u128,
    validity_priority_fee_divisor: u128,
    gas_estimate: GasEstimate,
    fresh_recipient_ratio: f64,
    signed_chunk_tx: mpsc::Sender<Vec<PresignedSenderBatch>>,
    nonce_reset_rx: mpsc::Receiver<SenderNonceReset>,
    validity_router: ValidityRouter,
    /// Query provider used to read the current block height per prepare round
    /// when resolving offset-based `block_number` validity predicates, and pending nonces
    /// on resync.
    query_client: QueryProvider,
}

#[derive(Debug)]
struct PresignedSenderBatch {
    sender_index: usize,
    generation: u64,
    txs: Vec<SignedTransaction>,
}

#[derive(Debug)]
struct SenderNonceReset {
    sender_index: usize,
    /// New generation that the producer must adopt after refetching the sender's pending nonce.
    generation: u64,
}

/// Enqueue-side presign state, owned by the coordinator task.
struct PresignEnqueueState<'a> {
    pipeline: &'a SubmissionPipeline,
    chunks: mpsc::Receiver<Vec<PresignedSenderBatch>>,
    nonce_resets: mpsc::Sender<SenderNonceReset>,
    buffer: PresignBuffer,
    presigned_generated: u64,
    offered_gas: u128,
    last_cycle_at: Option<Instant>,
    gas_estimate: GasEstimate,
    /// Per-sender free slots, reused across refill cycles.
    sender_slots: Vec<u64>,
}

impl PresignEnqueueState<'_> {
    fn buffer_chunk(&mut self, chunk: Vec<PresignedSenderBatch>) {
        for sender_batch in chunk {
            self.presigned_generated =
                self.presigned_generated.saturating_add(sender_batch.txs.len() as u64);
            self.buffer.push_sender_batch(
                sender_batch.sender_index,
                sender_batch.generation,
                sender_batch.txs,
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BlockAlignedEnqueueConfig {
    controller: MempoolDepthController,
    fallback_block_gas_limit: u64,
    block_time: Duration,
    /// Fixed refill cadence that replaces block-driven refills (`steady_submission_interval`).
    steady_submission_interval: Option<Duration>,
    presign_target_gas: u128,
    max_in_flight_per_sender: u64,
    max_total_in_flight: usize,
    deadline: Option<Instant>,
}

/// Constraint that limited a block-aligned injection plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectLimit {
    /// The configured depth target was fully satisfied.
    Target,
    /// No injection was required because depth already met the target.
    Nothing,
    /// Sender in-flight capacity prevented reaching the target.
    Capacity,
    /// The configured gas-per-second budget prevented reaching the target.
    Rate,
}

/// Gas budget produced for one block-aligned refill cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectPlan {
    /// Gas to take from the presign buffer and enqueue.
    pub inject_gas: u128,
    /// Desired submitted-but-unconfirmed gas after this cycle.
    pub desired_gas: u128,
    /// Current submitted-but-unconfirmed gas.
    pub depth_gas: u128,
    /// One-block gas floor, extended by canonical confirmation latency for rate targets.
    pub floor_gas: u128,
    /// Twice-floor gas ceiling, subject to the separate sender capacity limit.
    pub ceiling_gas: u128,
    /// Constraint that determined the plan.
    pub limited_by: InjectLimit,
}

/// Pure controller that keeps a bounded gas inventory sized for canonical confirmation latency.
#[derive(Debug, Clone, Copy)]
pub struct MempoolDepthController {
    confirmation_latency: Duration,
    target_gps: Option<u64>,
    block_time: Duration,
    capacity_gas: u128,
    measurement_started_at: Instant,
    /// Steady submission interval: send the rate budget each cycle instead of refilling to a
    /// depth target.
    steady_interval: Option<Duration>,
    /// When the previous refill cycle ran; bounds a steady cycle's catch-up.
    previous_cycle_at: Option<Instant>,
}

impl MempoolDepthController {
    /// Creates a controller for the measured run.
    pub const fn new(
        target_gps: Option<u64>,
        block_time: Duration,
        capacity_gas: u128,
        measurement_started_at: Instant,
    ) -> Self {
        Self {
            target_gps,
            block_time,
            capacity_gas,
            measurement_started_at,
            confirmation_latency: Duration::ZERO,
            steady_interval: None,
            previous_cycle_at: None,
        }
    }

    /// With a target rate, ignores the depth target so each cycle sends the gas due since the
    /// last one. The ceiling and sender capacity still bound what is in flight, and each cycle
    /// sends at most [`STEADY_CATCHUP_RATE`] times the gas due over the time since the previous
    /// cycle (one interval for the first), so a backlog left by a stall drains at a bounded rate
    /// instead of in one burst.
    pub const fn with_steady_submission(mut self, interval: Option<Duration>) -> Self {
        self.steady_interval = interval;
        self
    }

    /// Records when the previous refill cycle ran, for the steady catch-up bound.
    pub const fn with_previous_cycle(mut self, previous_cycle_at: Option<Instant>) -> Self {
        self.previous_cycle_at = previous_cycle_at;
        self
    }

    /// Replaces the sender capacity gas, for when the per-transaction gas estimate changes.
    pub const fn with_capacity_gas(mut self, capacity_gas: u128) -> Self {
        self.capacity_gas = capacity_gas;
        self
    }

    /// Uses canonical confirmation latency to budget a bounded in-flight window.
    pub const fn with_confirmation_latency(mut self, latency: Duration) -> Self {
        self.confirmation_latency = latency;
        self
    }

    /// Computes the gas to inject without performing I/O or reading a clock.
    pub fn plan(
        &self,
        now: Instant,
        block_gas_limit: u64,
        depth_gas: u128,
        confirmed_gas: u128,
        offered_gas: u128,
    ) -> InjectPlan {
        let floor_gas = self.target_gps.map_or_else(
            || u128::from(block_gas_limit),
            |target_gps| {
                u128::from(target_gps)
                    .saturating_mul(self.block_time.as_nanos())
                    .div_ceil(Duration::from_secs(1).as_nanos())
                    .min(u128::from(block_gas_limit))
            },
        );
        // Canonical observation can lag multiple short blocks. Reserve one extra block of
        // margin, but never grow the latency allowance beyond ten seconds of target load.
        let floor_gas = self.target_gps.map_or(floor_gas, |target_gps| {
            let horizon = self
                .confirmation_latency
                .saturating_add(self.block_time)
                .min(Duration::from_secs(10));
            floor_gas.max(
                u128::from(target_gps)
                    .saturating_mul(horizon.as_nanos())
                    .div_ceil(Duration::from_secs(1).as_nanos()),
            )
        });
        let ceiling_gas = floor_gas.saturating_mul(2);
        let catchup = self.target_gps.map_or(floor_gas, |target_gps| {
            let cumulative_target = u128::from(target_gps)
                .saturating_mul(
                    now.saturating_duration_since(self.measurement_started_at).as_nanos(),
                )
                .div_ceil(Duration::from_secs(1).as_nanos());
            cumulative_target.saturating_sub(confirmed_gas).min(floor_gas)
        });
        let desired_gas = if self.steady_interval.is_some() && self.target_gps.is_some() {
            ceiling_gas
        } else {
            floor_gas.saturating_add(catchup).min(ceiling_gas)
        };
        let wanted = desired_gas.saturating_sub(depth_gas);
        let capacity_headroom = self.capacity_gas.saturating_sub(depth_gas);
        let rate_headroom = self.target_gps.map_or(u128::MAX, |target_gps| {
            u128::from(target_gps)
                .saturating_mul(
                    now.saturating_duration_since(self.measurement_started_at).as_nanos(),
                )
                .div_ceil(Duration::from_secs(1).as_nanos())
                .saturating_sub(offered_gas)
        });
        let rate_headroom = match (self.steady_interval, self.target_gps) {
            (Some(interval), Some(target_gps)) => {
                let window = self
                    .previous_cycle_at
                    .map_or(interval, |previous| now.saturating_duration_since(previous));
                rate_headroom.min(
                    u128::from(target_gps)
                        .saturating_mul(window.as_nanos())
                        .saturating_mul(STEADY_CATCHUP_RATE)
                        .div_ceil(Duration::from_secs(1).as_nanos()),
                )
            }
            _ => rate_headroom,
        };
        let inject_gas = wanted.min(capacity_headroom).min(rate_headroom);
        let limited_by = if wanted == 0 {
            InjectLimit::Nothing
        } else if rate_headroom <= capacity_headroom && inject_gas < wanted {
            InjectLimit::Rate
        } else if inject_gas < wanted {
            InjectLimit::Capacity
        } else {
            InjectLimit::Target
        };

        InjectPlan { inject_gas, desired_gas, depth_gas, floor_gas, ceiling_gas, limited_by }
    }
}

impl LoadRunner {
    /// Computes the fixed transaction inventory required to hold the requested gas.
    pub fn mempool_target_transactions(
        target_gas: u64,
        average_gas: u64,
        capacity: u64,
    ) -> Result<u64> {
        if average_gas == 0 {
            return Err(BaselineError::Config("calibrated average gas must be > 0".into()));
        }
        let target = u128::from(target_gas).div_ceil(u128::from(average_gas));
        let target = u64::try_from(target)
            .map_err(|_| BaselineError::Config("mempool transaction target exceeds u64".into()))?;
        if target > capacity {
            warn!(
                target,
                capacity, "mempool target exceeds sender capacity; clamping to available capacity"
            );
        }
        Ok(target.min(capacity))
    }

    /// Runs the load test and returns metrics summary.
    pub async fn run(&mut self) -> Result<MetricsSummary> {
        if self.stop.is_cancelled() {
            return Err(BaselineError::Transaction("stopped before the load test started".into()));
        }
        if self.b20_run_salt.is_none()
            && self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
        {
            return Err(BaselineError::Config(
                "b20 run salt not set; call prepare_payloads before run".into(),
            ));
        }
        self.derive_reverse_swap_amounts().await?;

        self.collector.reset();

        let base_fee = self.client.get_base_fee().await?;
        info!(base_fee, "fetched current base fee");

        if !self.validity_router.is_disabled() {
            self.probe_validity_endpoint().await?;
        }

        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|account| account.address).collect();
        let sender_count = sender_addresses.len();
        let (events, inbox) = mpsc::channel(RUN_EVENT_BUFFER);
        let (base_fee_tx, base_fee_rx) = watch::channel(base_fee);

        // Watchers outlive a stop request: the confirmation drain still needs their blocks.
        let receipt_provider = RootProvider::<Base>::new_http(self.config.query_rpc.clone());
        let watcher_cancel = CancellationToken::new();
        let _watcher_cancel_guard = watcher_cancel.clone().drop_guard();
        let block_watcher_task = BlockWatcher::new(
            receipt_provider.clone(),
            self.config.block_time,
            events.clone(),
            base_fee_tx,
            watcher_cancel.clone(),
        )
        .start();
        let canonical_head_watcher_task = self.config.canonical_heads_ws.clone().map(|ws_url| {
            CanonicalHeadWatcher::new(ws_url, events.clone(), watcher_cancel.clone()).start()
        });
        let (receipt_sampler, receipt_sampler_task) = BlockWatcher::start_receipt_sampler(
            receipt_provider.clone(),
            events.clone(),
            watcher_cancel.clone(),
        );

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let initial_avg_gas = self.calibrate_avg_gas().await?;
        let gas_estimate = GasEstimate::new(initial_avg_gas);
        // Seed the collector so live throughput (rolling GPS) has a non-zero
        // gas figure before canonical receipt gas lands.
        self.collector.set_estimated_gas(initial_avg_gas);
        let mut start = Instant::now();

        info!(
            sender_count,
            sender_worker_count = SubmissionPipeline::sender_worker_count(
                self.submission_batch_rpcs.len(),
                self.config.max_concurrent_submit_requests,
            ),
            submit_rpc_count = self.submission_batch_rpcs.len(),
            max_concurrent_submit_requests =
                self.config.max_concurrent_submit_requests.unwrap_or_default(),
            batch_size = self.config.batch_size,
            max_in_flight_per_sender,
            initial_avg_gas,
            target_gps = self.config.target_gps.unwrap_or_default(),
            duration_secs =
                self.config.duration.map(|duration| duration.as_secs()).unwrap_or_default(),
            block_time_ms = self.config.block_time.as_millis(),
            "load test started"
        );

        let mut submission_pipeline = SubmissionPipeline::start(
            Arc::clone(&self.submission_batch_rpcs),
            events,
            PipelineStartConfig {
                validity_signing: self.config.validity_signing,
                max_concurrent_submit_requests: self.config.max_concurrent_submit_requests,
            },
        );

        let mut open_loop_enqueue_error: Option<BaselineError> = None;
        let pre_sign_started = Instant::now();
        let nonce_initialization_started = Instant::now();
        let sender_start_nonces = Self::pending_nonces(&self.client, &sender_addresses).await?;
        info!(
            sender_count,
            elapsed_ms = nonce_initialization_started.elapsed().as_millis(),
            "sender nonces initialized"
        );

        let capacity = self.config.effective_in_flight_capacity(sender_count);
        if let Some(max_total) = self.config.max_total_in_flight {
            info!(
                max_total_in_flight = max_total,
                effective_capacity = capacity,
                "capping open-loop in-flight target with max_total_in_flight"
            );
        }
        let block_gas_limit = if let Some(limit) = self.config.block_gas_limit {
            limit
        } else {
            self.client
                .get_block_by_number(BlockNumberOrTag::Latest)
                .hashes()
                .await
                .map_err(|e| {
                    BaselineError::Rpc(format!("failed to read latest block gas limit: {e}"))
                })?
                .ok_or_else(|| BaselineError::Rpc("latest block is unavailable".into()))?
                .header
                .gas_limit
        };
        let requested_floor_gas = self.config.target_gps.map_or_else(
            || u128::from(block_gas_limit),
            |target_gps| {
                u128::from(target_gps)
                    .saturating_mul(self.config.block_time.as_nanos())
                    .div_ceil(Duration::from_secs(1).as_nanos())
            },
        );
        let floor_gas = requested_floor_gas.min(u128::from(block_gas_limit));
        if requested_floor_gas > floor_gas {
            warn!(
                requested_floor_gas,
                block_gas_limit,
                "per-block gas target exceeds block gas limit; clamping to block capacity"
            );
        }
        let initial_target_in_flight = Self::mempool_target_transactions(
            u64::try_from(floor_gas).unwrap_or(u64::MAX),
            initial_avg_gas,
            capacity as u64,
        )?;
        let capacity_gas = u128::from(capacity as u64).saturating_mul(u128::from(initial_avg_gas));

        let replacement_generator = WorkloadGenerator::from_tx_configs(
            self.workload_config(),
            &self.config.transactions,
            self.b20_run_salt,
        )?;
        let producer_generator = std::mem::replace(&mut self.generator, replacement_generator);
        let lead_payload = self
            .config
            .validity_priority_lead_payload
            .as_ref()
            .map(|tx_type| SelectedPayload::from_tx_type(tx_type, self.b20_run_salt));
        let producer_recipient_keys = self.recipient_keys.take();
        let producer_recipient_rng = std::mem::take(&mut self.recipient_rng);

        let (signed_chunk_tx, signed_chunk_rx) = mpsc::channel(PRESIGN_CHANNEL_BUFFER);
        let (nonce_reset_tx, nonce_reset_rx) = mpsc::channel(sender_count.max(1));
        let producer_task = tokio::spawn(Self::stream_presigned_transactions(
            PresignProducerState {
                generator: producer_generator,
                lead_payload,
                recipient_keys: producer_recipient_keys,
                recipient_rng: producer_recipient_rng,
            },
            PresignConfig {
                senders: sender_addresses
                    .iter()
                    .zip(sender_start_nonces)
                    .map(|(&address, next_nonce)| ProducerSender {
                        address,
                        next_nonce,
                        generation: 0,
                    })
                    .collect(),
                signers: self.accounts.accounts().iter().map(|a| a.signer.clone()).collect(),
                chain_id: self.config.chain_id,
                base_fee_rx: base_fee_rx.clone(),
                max_gas_price: self.config.max_gas_price,
                validity_priority_lead_multiplier: self.config.validity_priority_lead_multiplier,
                validity_priority_fee_divisor: self.config.validity_priority_fee_divisor,
                gas_estimate: gas_estimate.clone(),
                fresh_recipient_ratio: self.config.fresh_recipient_ratio,
                signed_chunk_tx,
                nonce_reset_rx,
                validity_router: self.validity_router.clone(),
                query_client: self.client.clone(),
            },
        ));

        let mut coordinator = RunCoordinator::new(
            inbox,
            ResultsTracker::new(&sender_addresses),
            &mut self.collector,
            receipt_sampler,
            ProgressReport {
                display: self.display.as_ref(),
                stage: LoadTestStage::Prefill,
                start,
                duration: None,
                max_in_flight_per_sender: max_in_flight_per_sender as u64,
                account_count: sender_count,
                target_gps: self.config.target_gps,
                base_fee: base_fee_rx,
                last_log: Instant::now(),
            },
        );
        // Render immediately so the live counters appear before the first confirmation.
        coordinator.report();

        let mut enqueue_state = PresignEnqueueState {
            pipeline: &submission_pipeline,
            chunks: signed_chunk_rx,
            nonce_resets: nonce_reset_tx,
            buffer: PresignBuffer::new(sender_count),
            presigned_generated: 0,
            offered_gas: 0,
            last_cycle_at: None,
            gas_estimate,
            sender_slots: Vec::with_capacity(sender_count),
        };
        let prefill_result = Self::enqueue_prefill_transactions(
            &mut enqueue_state,
            &mut coordinator,
            floor_gas.min(capacity_gas),
            max_in_flight_per_sender as u64,
            capacity,
            Instant::now() + PREFILL_TIMEOUT,
            &self.stop,
        )
        .await;

        if prefill_result.is_ok()
            && !coordinator
                .pump_while(Instant::now() + SUBMIT_DRAIN_TIMEOUT, |c| {
                    c.outstanding_batches() > 0 && !self.stop.is_cancelled()
                })
                .await
        {
            warn!(
                pending_batches = coordinator.outstanding_batches(),
                "warmup submission pipeline is still draining; continuing as a measured bottleneck"
            );
        }

        let enqueue_result = match prefill_result {
            Err(err) => Err(err),
            Ok(()) => {
                async {
                    let control_dir = self.config.separate_setup.as_deref();
                    Self::publish_handshake(control_dir.map(|dir| dir.join("ready")).as_deref())?;
                    Self::wait_for_start_file(control_dir, &mut coordinator, &self.stop).await?;
                    let measurement_start_block = coordinator
                        .pump_until(receipt_provider.get_block_number())
                        .await
                        .map_err(|e| {
                            BaselineError::Rpc(format!(
                                "failed to read latest block number for measurement start: {e}"
                            ))
                        })?;
                    coordinator
                        .tracker
                        .begin_measurement(measurement_start_block, self.config.measurement_blocks);
                    coordinator.collector.reset();
                    coordinator.collector.set_estimated_gas(initial_avg_gas);
                    start = Instant::now();
                    Self::publish_handshake(control_dir.map(|dir| dir.join("started")).as_deref())?;

                    coordinator.set_stage(LoadTestStage::Submitting);
                    coordinator.report.start = start;
                    coordinator.report.duration = self.config.duration;
                    info!(
                        initial_target_in_flight,
                        duration_secs = ?self.config.duration.map(|d| d.as_secs()),
                        "starting open-loop measured enqueue"
                    );
                    Self::enqueue_block_aligned_transactions(
                        &mut enqueue_state,
                        BlockAlignedEnqueueConfig {
                            controller: MempoolDepthController::new(
                                self.config.target_gps,
                                self.config.block_time,
                                capacity_gas,
                                start,
                            )
                            .with_steady_submission(self.config.steady_submission_interval),
                            fallback_block_gas_limit: block_gas_limit,
                            block_time: self.config.block_time,
                            steady_submission_interval: self.config.steady_submission_interval,
                            presign_target_gas: floor_gas.saturating_mul(2).min(capacity_gas),
                            max_in_flight_per_sender: max_in_flight_per_sender as u64,
                            max_total_in_flight: capacity,
                            deadline: self.config.duration.map(|d| start + d),
                        },
                        &mut coordinator,
                        &self.stop,
                    )
                    .await
                }
                .await
            }
        };
        coordinator.collector.set_pacing_duration(start.elapsed());

        let finished_file = self.config.separate_setup.as_deref().map(|dir| dir.join("finished"));
        Self::publish_handshake(finished_file.as_deref())?;

        let (presigned_generated, offered_gas) =
            (enqueue_state.presigned_generated, enqueue_state.offered_gas);
        drop(enqueue_state);
        match coordinator.pump_until(producer_task).await {
            Ok(Ok(producer_state)) => {
                self.generator = producer_state.generator;
                self.recipient_keys = producer_state.recipient_keys;
                self.recipient_rng = producer_state.recipient_rng;
            }
            Ok(Err(err)) => {
                warn!(error = %err, "open-loop pre-sign producer failed");
                open_loop_enqueue_error.get_or_insert(err);
            }
            Err(join_error) => {
                let err = BaselineError::Transaction(format!(
                    "open-loop pre-sign producer task failed: {join_error}"
                ));
                warn!(error = %err, "open-loop pre-sign producer task failed");
                open_loop_enqueue_error.get_or_insert(err);
            }
        }

        if let Err(err) = enqueue_result {
            warn!(
                error = %err,
                presigned_generated,
                "open-loop enqueue aborted; proceeding to drain and shutdown"
            );
            open_loop_enqueue_error = Some(err);
        } else {
            info!(
                submitted = coordinator.collector.submitted_count(),
                offered_gas,
                elapsed_secs = pre_sign_started.elapsed().as_secs_f64(),
                "load test enqueue complete"
            );
        }

        submission_pipeline.close_input();
        let stop_at_measurement_cutoff = self.config.measurement_blocks.is_some();
        if !stop_at_measurement_cutoff {
            coordinator
                .pump_while(Instant::now() + SUBMIT_DRAIN_TIMEOUT, |c| c.outstanding_batches() > 0)
                .await;
        }
        let pending_submit_batches = coordinator.outstanding_batches();
        if pending_submit_batches > 0 {
            if stop_at_measurement_cutoff {
                warn!(pending_submit_batches, "closing submit queue at measured block cutoff");
            } else {
                warn!(
                    pending_submit_batches,
                    "timed out waiting for submit queue to drain, closing submit queue"
                );
            }
            coordinator.abandon(submission_pipeline.abandon_queued(), "submit queue abandoned");
        }
        coordinator
            .pump_until(submission_pipeline.shutdown_and_join(SUBMIT_WORKER_SHUTDOWN_TIMEOUT))
            .await;
        drop(submission_pipeline);
        coordinator.drain_inbox();

        coordinator.set_stage(LoadTestStage::DrainingConfirmations);

        let submitted = coordinator.collector.submitted_count();
        let elapsed = start.elapsed();
        info!(
            submitted,
            in_flight = coordinator.tracker.total_in_flight(),
            elapsed_secs = elapsed.as_secs(),
            actual_tps = submitted as f64 / elapsed.as_secs_f64(),
            "load test complete, draining confirmations"
        );

        let drain_began = start.elapsed();
        let last_confirmed = if open_loop_enqueue_error.is_some() {
            warn!("skipping confirmation drain after open-loop enqueue failure");
            let (undrained, undrained_gas) = coordinator.tracker.drain_pending();
            coordinator.collector.record_undrained_inventory(undrained, undrained_gas);
            None
        } else {
            coordinator.drain_confirmations(CONFIRMATION_DRAIN_TIMEOUT, DRAIN_STALL_TIMEOUT).await
        };
        let last_confirmed_at =
            last_confirmed.map_or(drain_began, |at| at.saturating_duration_since(start));

        watcher_cancel.cancel();
        let watchers = [
            ("block watcher", Some(block_watcher_task)),
            ("canonical head watcher", canonical_head_watcher_task),
            ("receipt sampler", Some(receipt_sampler_task)),
        ];
        for (watcher, task) in watchers {
            let Some(task) = task else {
                continue;
            };
            if let Ok(Err(error)) =
                coordinator.pump_until(tokio::time::timeout(WATCHER_SHUTDOWN_TIMEOUT, task)).await
                && error.is_panic()
            {
                warn!(watcher, error = %error, "watcher panicked");
            }
        }

        let confirmed = coordinator.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        // Fetch canonical receipts in a single batch pass, scoped to only the blocks
        // our transactions landed in, to backfill gas and revert status. This can be
        // slow on large runs, so notify the user before starting.
        let landed_blocks = coordinator.tracker.landed_block_numbers();
        if open_loop_enqueue_error.is_none() && !landed_blocks.is_empty() {
            println!(
                "Fetching receipts for {} block(s) to compute gas and reverts (this may take a while)...",
                landed_blocks.len()
            );
            let receipt_fetch_start = Instant::now();
            let (receipts, failed_blocks) =
                BlockWatcher::fetch_receipts(&receipt_provider, &landed_blocks).await;
            let receipts_by_hash: HashMap<TxHash, _> =
                receipts.into_iter().map(|receipt| (receipt.tx_hash, receipt)).collect();
            coordinator.collector.apply_receipts(
                &receipts_by_hash,
                landed_blocks.len(),
                failed_blocks,
            );
            info!(
                blocks = landed_blocks.len(),
                failed_blocks,
                receipts = receipts_by_hash.len(),
                elapsed_secs = receipt_fetch_start.elapsed().as_secs_f64(),
                "end-of-run receipt pass complete"
            );
        }
        let measurement_window = coordinator.tracker.measurement_window();
        drop(coordinator);

        let mut summary = self.collector.summarize_with_fresh_recipient_count(
            last_confirmed_at,
            self.config_summary.clone(),
            self.fresh_recipient_count(),
        );
        summary.measurement_start_block = measurement_window.start_block;
        summary.measurement_end_block = measurement_window.end_block;
        summary.measurement_block_count = measurement_window.block_count;
        if let Some(fresh_recipient_count) = summary.fresh_recipient_count {
            info!(fresh_recipient_count, "fresh recipient generation complete");
        }

        if let Some(err) = open_loop_enqueue_error {
            // Stash the real stats gathered so far (submitted/confirmed/failed counts, gas,
            // latencies) so callers can present them alongside the error instead of an
            // all-zero summary — plenty of transactions may have landed before the failure.
            summary.error = Some(err.to_string());
            self.partial_summary = Some(summary);
            return Err(err);
        }
        Ok(summary)
    }

    /// Waits for the external `start` handshake file while the coordinator keeps draining.
    async fn wait_for_start_file(
        control_dir: Option<&Path>,
        coordinator: &mut RunCoordinator<'_>,
        stop: &CancellationToken,
    ) -> Result<()> {
        let Some(control_dir) = control_dir else {
            return Ok(());
        };
        let path = control_dir.join("start");
        let deadline = Instant::now() + START_FILE_TIMEOUT;
        // ponytail: polls the filesystem; a file appearing has no portable async notification.
        let mut poll = tokio::time::interval(START_FILE_POLL_INTERVAL);
        while !path.exists() {
            if stop.is_cancelled() {
                return Err(BaselineError::Transaction(
                    "stopped while waiting for start handshake".into(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(BaselineError::Timeout {
                    operation: "start handshake file".into(),
                    duration: START_FILE_TIMEOUT,
                });
            }
            coordinator.pump_until(poll.tick()).await;
        }
        Ok(())
    }

    /// Reads a sender's pending nonce, which follows transactions the node already holds.
    /// Bounded by the query client's [`RPC_TIMEOUT`](crate::rpc::RPC_TIMEOUT).
    async fn pending_nonce(client: &QueryProvider, sender: Address) -> Result<u64> {
        // The transport error can embed a credential-bearing RPC URL, so it is not formatted.
        client.get_transaction_count(sender).pending().await.map_err(|_| {
            BaselineError::Rpc(format!("failed to fetch pending nonce for sender {sender}"))
        })
    }

    async fn pending_nonces(client: &QueryProvider, senders: &[Address]) -> Result<Vec<u64>> {
        stream::iter(senders.iter().map(|&sender| Self::pending_nonce(client, sender)))
            .buffered(super::funding::FUNDING_CONCURRENCY)
            .try_collect()
            .await
    }

    fn select_recipient(
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        fresh_recipient_ratio: f64,
        sender_pool_recipient: Address,
    ) -> Result<Address> {
        let Some(recipient_keys) = recipient_keys.as_mut() else {
            return Ok(sender_pool_recipient);
        };

        if fresh_recipient_ratio >= 1.0 || recipient_rng.random::<f64>() < fresh_recipient_ratio {
            Ok(recipient_keys.next_signer()?.address())
        } else {
            Ok(sender_pool_recipient)
        }
    }

    /// Reads the latest canonical block number, used to resolve offset-based
    /// `block_number` validity predicates at prepare time.
    async fn current_block_height(client: &QueryProvider) -> Result<u64> {
        let block = client
            .get_block_by_number(BlockNumberOrTag::Latest)
            .hashes()
            .await
            .map_err(|e| BaselineError::Rpc(format!("failed to read latest block number: {e}")))?
            .ok_or_else(|| BaselineError::Rpc("latest block is unavailable".into()))?;
        Ok(block.header.number)
    }

    /// Prepares `txs_per_sender` requests per sender. Priority-lead validity senders draw from
    /// `lead_payload` when one is configured; every other sender draws from the weighted mix.
    fn build_sender_jobs(
        generator: &mut WorkloadGenerator,
        lead_payload: Option<&SelectedPayload>,
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        config: &PresignConfig,
        txs_per_sender: usize,
        current_block: u64,
    ) -> Result<Vec<SenderJob>> {
        let sender_count = config.senders.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }
        let mut sender_jobs = Vec::with_capacity(sender_count);
        for (sender_index, sender) in config.senders.iter().enumerate() {
            let from = sender.address;
            let ring_recipient = config.senders[(sender_index + 1) % sender_count].address;
            let pair_index = Self::b20_partner_index(sender_index, sender_count);
            let pair_recipient = config.senders[pair_index].address;
            let cohort = config.validity_router.cohort_for_sender(from);
            let fixed_payload = match cohort {
                SubmitCohort::ValidityPassPriorityLead => lead_payload,
                SubmitCohort::Plain | SubmitCohort::ValidityPass => None,
            };
            let mut prepared_txs = Vec::with_capacity(txs_per_sender);
            for _ in 0..txs_per_sender {
                let payload = match fixed_payload {
                    Some(payload) => payload.clone(),
                    None => generator.select_payload()?,
                };
                let sender_pool_recipient =
                    if payload.uses_pair_recipient() { pair_recipient } else { ring_recipient };
                let to = if payload.uses_runner_recipient() {
                    Self::select_recipient(
                        recipient_keys,
                        recipient_rng,
                        config.fresh_recipient_ratio,
                        sender_pool_recipient,
                    )?
                } else {
                    sender_pool_recipient
                };

                let tx_request = generator.generate_selected_payload(&payload, from, to);
                let to_addr = tx_request.to.and_then(|kind| kind.to().copied());
                let value = tx_request.value.unwrap_or(U256::ZERO);
                let data = tx_request.input.input().cloned().unwrap_or_default();
                let gas_limit = tx_request.gas.unwrap_or(21_000);
                let validity =
                    config.validity_router.predicates_for(cohort, current_block, from, to_addr);

                prepared_txs.push(PreparedTransaction {
                    native: tx_request.transaction_type == Some(Eip8130Constants::EIP8130_TX_TYPE),
                    from,
                    to: to_addr,
                    value,
                    data,
                    gas_limit,
                    estimated_gas: config.gas_estimate.get(),
                    validity,
                    cohort,
                });
            }

            sender_jobs.push(SenderJob {
                sender_index,
                from,
                start_nonce: sender.next_nonce,
                generation: sender.generation,
                prepared_txs,
            });
        }

        Ok(sender_jobs)
    }

    async fn sign_sender_jobs(
        sender_jobs: Vec<SenderJob>,
        signers: Arc<[PrivateKeySigner]>,
        chain_id: u64,
        base_fee: u128,
        max_gas_price: u128,
        validity_priority_lead_multiplier: u128,
        validity_priority_fee_divisor: u128,
    ) -> Result<Vec<PresignedSenderBatch>> {
        let sender_count = sender_jobs.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }

        // Signing is CPU-bound. Chunking work to the available cores avoids growing Tokio's
        // blocking pool to one thread per sender under high-sender-count stress workloads.
        let signing_worker_count =
            std::thread::available_parallelism().map(usize::from).unwrap_or(1).min(sender_count);
        let jobs_per_task = sender_count.div_ceil(signing_worker_count);
        let mut sender_jobs = sender_jobs.into_iter();
        let mut signing_tasks = Vec::with_capacity(signing_worker_count);
        loop {
            let jobs: Vec<SenderJob> = sender_jobs.by_ref().take(jobs_per_task).collect();
            if jobs.is_empty() {
                break;
            }
            let signers = Arc::clone(&signers);
            signing_tasks.push(task::spawn_blocking(move || {
                jobs.into_iter()
                    .map(|sender_job| {
                        let signer = &signers[sender_job.sender_index];
                        Self::sign_sender_job(
                            sender_job,
                            signer,
                            chain_id,
                            base_fee,
                            max_gas_price,
                            validity_priority_lead_multiplier,
                            validity_priority_fee_divisor,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            }));
        }

        // Jobs are built in sender order and each task keeps its slice in order.
        let mut chunk = Vec::with_capacity(sender_count);
        for signing_task in signing_tasks {
            chunk.extend(signing_task.await.map_err(|e| {
                BaselineError::Transaction(format!("open-loop signing task failed: {e}"))
            })??);
        }
        Ok(chunk)
    }

    async fn stream_presigned_transactions(
        mut producer_state: PresignProducerState,
        mut config: PresignConfig,
    ) -> Result<PresignProducerState> {
        if config.senders.is_empty() {
            return Ok(producer_state);
        }

        let chunk_per_sender = (SIGNED_BATCH_SIZE / config.senders.len()).max(1);
        let nonce_increment = chunk_per_sender as u64;

        // The producer streams indefinitely and only stops when the consumer drops
        // `signed_chunk_rx` (detected below via the `send(...).is_err()` check), which
        // happens once the enqueue loop returns (deadline reached or channel closed).
        loop {
            // Terminal rejections create a local nonce gap for already-presigned work. Apply reset
            // requests before building the next chunk so affected senders resume from the node's
            // pending nonce instead of being removed from the active sender set.
            Self::apply_nonce_resets(&mut config).await?;

            // Resolve offset-based block_number predicates against the chain
            // height once per prepare round (not per transaction). Skip the
            // extra read entirely unless an offset bound is actually configured,
            // so absolute-only (and non-validity) runs keep their prior behavior.
            let current_block = if config.validity_router.needs_current_block() {
                Self::current_block_height(&config.query_client).await?
            } else {
                0
            };
            let sender_jobs = Self::build_sender_jobs(
                &mut producer_state.generator,
                producer_state.lead_payload.as_ref(),
                &mut producer_state.recipient_keys,
                &mut producer_state.recipient_rng,
                &config,
                chunk_per_sender,
                current_block,
            )?;

            let base_fee = *config.base_fee_rx.borrow_and_update();
            let chunk = Self::sign_sender_jobs(
                sender_jobs,
                Arc::clone(&config.signers),
                config.chain_id,
                base_fee,
                config.max_gas_price,
                config.validity_priority_lead_multiplier,
                config.validity_priority_fee_divisor,
            )
            .await?;

            if config.signed_chunk_tx.send(chunk).await.is_err() {
                break;
            }

            for (sender_index, sender) in config.senders.iter_mut().enumerate() {
                sender.next_nonce = sender.next_nonce.checked_add(nonce_increment).ok_or_else(|| {
                    BaselineError::Transaction(format!(
                        "nonce overflow while advancing open-loop producer sender index {sender_index}"
                    ))
                })?;
            }
        }

        Ok(producer_state)
    }

    async fn apply_nonce_resets(config: &mut PresignConfig) -> Result<()> {
        while let Ok(reset) = config.nonce_reset_rx.try_recv() {
            let Some(sender) = config.senders.get_mut(reset.sender_index) else {
                continue;
            };
            if sender.generation >= reset.generation {
                continue;
            }
            // The pending nonce restarts after transactions the node already accepted while
            // skipping the rejected stale tail.
            sender.next_nonce = Self::pending_nonce(&config.query_client, sender.address).await?;
            sender.generation = reset.generation;
            info!(
                sender = %sender.address,
                sender_index = reset.sender_index,
                generation = reset.generation,
                next_nonce = sender.next_nonce,
                "resynced open-loop sender after terminal rejection"
            );
        }

        Ok(())
    }

    fn sign_sender_job(
        sender_job: SenderJob,
        signer: &PrivateKeySigner,
        chain_id: u64,
        base_fee: u128,
        max_gas_price: u128,
        validity_priority_lead_multiplier: u128,
        validity_priority_fee_divisor: u128,
    ) -> Result<PresignedSenderBatch> {
        let pricer = GasPricer::new(max_gas_price);
        let mut signed_txs = Vec::with_capacity(sender_job.prepared_txs.len());

        for (nonce_offset, prepared) in sender_job.prepared_txs.into_iter().enumerate() {
            let nonce_offset = u64::try_from(nonce_offset).map_err(|e| {
                BaselineError::Transaction(format!("failed to convert nonce offset to u64: {e}"))
            })?;
            let nonce = sender_job.start_nonce.checked_add(nonce_offset).ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "nonce overflow for sender {} at offset {nonce_offset}",
                    sender_job.from
                ))
            })?;

            let fees = pricer.fees_for_cohort(
                base_fee,
                prepared.cohort,
                validity_priority_fee_divisor,
                validity_priority_lead_multiplier,
            );
            let mut signed =
                SubmissionPipeline::sign_at_nonce(signer, &prepared, chain_id, nonce, fees)?;
            // Authorized up front so sender workers never hold keys; attached only when the
            // signing mode or the endpoint asks for it.
            signed.authorize_validity(signer, chain_id)?;
            signed_txs.push(signed);
        }

        Ok(PresignedSenderBatch {
            sender_index: sender_job.sender_index,
            generation: sender_job.generation,
            txs: signed_txs,
        })
    }

    /// Splits `selected` into submit batches and enqueues them. A closed queue means the run is
    /// ending, so the unqueued remainder is dropped.
    async fn enqueue_selected(
        pipeline: &SubmissionPipeline,
        coordinator: &mut RunCoordinator<'_>,
        selected: Vec<SignedTransaction>,
        measured: bool,
    ) {
        let mut txs = selected.into_iter();
        loop {
            let batch: Vec<_> = txs.by_ref().take(SIGNED_BATCH_SIZE).collect();
            if batch.is_empty() || !coordinator.enqueue(pipeline, batch, measured).await {
                return;
            }
        }
    }

    async fn enqueue_block_aligned_transactions(
        enqueue_state: &mut PresignEnqueueState<'_>,
        config: BlockAlignedEnqueueConfig,
        coordinator: &mut RunCoordinator<'_>,
        stop: &CancellationToken,
    ) -> Result<()> {
        let safety_interval = config.steady_submission_interval.unwrap_or_else(|| {
            (config.block_time / 4).clamp(Duration::from_millis(1), Duration::from_millis(250))
        });
        let mut safety_tick = tokio::time::interval(safety_interval);
        safety_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let deadline = futures::future::OptionFuture::from(
            config.deadline.map(|deadline| tokio::time::sleep_until(deadline.into())),
        );
        tokio::pin!(deadline);
        let mut last_block_gas_limit = config.fallback_block_gas_limit;
        // Canonical RPC observations are routinely delayed beyond a 200ms block interval. Keep
        // submitting at the finer safety cadence until one arrives, otherwise a delayed
        // observation holds the next refill for 300ms and makes transactions miss the payload
        // build window for the following slot.
        let fallback_refill_interval = safety_interval;
        let mut last_pulse_at =
            Instant::now().checked_sub(fallback_refill_interval).unwrap_or_else(Instant::now);
        let mut last_recorded_canonical_block =
            coordinator.tracker.measurement_window().start_block;

        loop {
            coordinator.drain_inbox();
            if stop.is_cancelled() || config.deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(());
            }
            let finished = coordinator.tracker.measurement_finished();
            if finished
                && last_recorded_canonical_block
                    .zip(coordinator.tracker.measurement_window().end_block)
                    .is_some_and(|(recorded, end)| recorded >= end)
            {
                return Ok(());
            }

            // After measurement ends, canonical blocks are recorded without refilling.
            if let Some(pulse) = coordinator.pop_pulse() {
                last_recorded_canonical_block = Some(pulse.number);
                if finished {
                    Self::record_terminal_canonical_cycle(pulse, &config, coordinator);
                    continue;
                }
                last_block_gas_limit = pulse.gas_limit;
                last_pulse_at = pulse.observed_at;
                Self::run_refill_cycle(
                    enqueue_state,
                    RefillTrigger::Canonical(pulse),
                    last_block_gas_limit,
                    &config,
                    coordinator,
                )
                .await;
                continue;
            }

            let buffer_wanted =
                !finished && enqueue_state.buffer.buffered_gas() < config.presign_target_gas;
            tokio::select! {
                biased;
                () = stop.cancelled() => return Ok(()),
                _ = &mut deadline, if config.deadline.is_some() => return Ok(()),
                alive = coordinator.next_event() => if !alive { return Ok(()) },
                _ = safety_tick.tick(), if !finished => {
                    // Steady submission refills on every tick, so arrivals do not depend on
                    // when canonical blocks are observed.
                    if config.steady_submission_interval.is_some()
                        || last_pulse_at.elapsed() >= fallback_refill_interval
                    {
                        Self::run_refill_cycle(
                            enqueue_state,
                            RefillTrigger::Safety(Instant::now()),
                            last_block_gas_limit,
                            &config,
                            coordinator,
                        )
                        .await;
                        last_pulse_at = Instant::now();
                    }
                }
                chunk = enqueue_state.chunks.recv(), if buffer_wanted => {
                    let Some(chunk) = chunk else {
                        return Ok(());
                    };
                    enqueue_state.buffer_chunk(chunk);
                }
            }
        }
    }

    /// Records the canonical block that closes a block-count measurement without refilling the
    /// submission pipeline after the measurement window has ended.
    fn record_terminal_canonical_cycle(
        canonical: BlockPulse,
        config: &BlockAlignedEnqueueConfig,
        coordinator: &mut RunCoordinator<'_>,
    ) {
        let plan_started = Instant::now();
        let depth_gas = coordinator.tracker.unconfirmed_gas();
        let confirmed_gas = coordinator.tracker.confirmed_gas();
        let queued_gas = coordinator.queued_gas();
        let (_, canonical_p99) = coordinator.collector.rolling_p50_p99();
        let plan = config.controller.with_confirmation_latency(canonical_p99).plan(
            canonical.observed_at,
            canonical.gas_limit,
            depth_gas,
            confirmed_gas,
            0,
        );
        let plan_time = plan_started.elapsed();
        coordinator.collector.record_pacing_cycle(PacingCycleObservation {
            elapsed: canonical
                .observed_at
                .saturating_duration_since(config.controller.measurement_started_at),
            source: PacingCycleSource::Canonical,
            block_observed: true,
            block_gas_used: canonical.gas_used,
            block_gas_limit: canonical.gas_limit,
            our_included_gas: canonical.our_included_gas,
            pre_refill_depth_gas: depth_gas,
            post_refill_depth_gas: depth_gas,
            queued_gas,
            floor_gas: plan.floor_gas,
            offered_gas: 0,
            capacity_limited: false,
            chain_bound: depth_gas >= plan.ceiling_gas,
            presign_starved: false,
            availability_lag: Some(
                canonical.observed_at.saturating_duration_since(canonical.expected_boundary),
            ),
            plan_time,
            submit_time: None,
            refill_lag: None,
        });
    }

    /// Resets senders whose submissions failed: their buffered work is stale and the producer
    /// must restart them from the node's pending nonce.
    fn reset_rejected_senders(
        enqueue_state: &mut PresignEnqueueState<'_>,
        coordinator: &mut RunCoordinator<'_>,
    ) {
        let mut reset_sender_count = 0usize;
        for sender_index in coordinator.take_rejected_senders() {
            let Some(generation) = enqueue_state.buffer.reset_sender(sender_index) else {
                continue;
            };
            // The coordinator owns the presign buffer and generation filter, while the producer
            // owns future nonce assignment. A full queue means the producer has not caught up
            // yet; the sender is retried next cycle and its newer generation supersedes.
            match enqueue_state.nonce_resets.try_send(SenderNonceReset { sender_index, generation })
            {
                Ok(()) => reset_sender_count += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    coordinator.requeue_rejected_sender(sender_index);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        sender_index,
                        generation, "presign producer stopped before sender nonce reset"
                    );
                }
            }
        }
        if reset_sender_count > 0 {
            warn!(reset_sender_count, "reset senders after terminal nonce rejection");
        }
    }

    async fn run_refill_cycle(
        enqueue_state: &mut PresignEnqueueState<'_>,
        trigger: RefillTrigger,
        fallback_block_gas_limit: u64,
        config: &BlockAlignedEnqueueConfig,
        coordinator: &mut RunCoordinator<'_>,
    ) {
        let cycle_started = trigger.observed_at();
        let canonical = trigger.block();
        while enqueue_state.buffer.buffered_gas() < config.presign_target_gas {
            let Ok(chunk) = enqueue_state.chunks.try_recv() else {
                break;
            };
            enqueue_state.buffer_chunk(chunk);
        }
        coordinator.drain_inbox();
        Self::reset_rejected_senders(enqueue_state, coordinator);

        let plan_started = Instant::now();
        let (observed_gas, observed_count) = coordinator.tracker.observed_gas();
        let previous_estimate = enqueue_state.gas_estimate.get();
        let estimated_gas = enqueue_state.gas_estimate.update(observed_gas, observed_count);
        if estimated_gas != previous_estimate {
            coordinator.collector.set_estimated_gas(estimated_gas);
            info!(
                estimated_gas,
                previous_estimate, observed_count, "corrected pacing gas estimate from receipt gas"
            );
        }
        // Local submission backlog has not reached the node yet. Counting it as mempool
        // inventory can starve the RPC pipeline while acknowledgements lag.
        let depth_gas = coordinator.tracker.unconfirmed_gas();
        let confirmed_gas = coordinator.tracker.confirmed_gas();
        let block_gas_limit = canonical.map_or(fallback_block_gas_limit, |block| block.gas_limit);
        let (_, canonical_p99) = coordinator.collector.rolling_p50_p99();
        let plan = config
            .controller
            .with_capacity_gas(
                u128::from(config.max_total_in_flight as u64)
                    .saturating_mul(u128::from(estimated_gas)),
            )
            .with_confirmation_latency(canonical_p99)
            .with_previous_cycle(enqueue_state.last_cycle_at)
            .plan(
                cycle_started,
                block_gas_limit,
                depth_gas,
                confirmed_gas,
                enqueue_state.offered_gas,
            );
        enqueue_state.last_cycle_at = Some(cycle_started);
        let buffered_before = enqueue_state.buffer.buffered_gas();
        coordinator.sender_slots(config.max_in_flight_per_sender, &mut enqueue_state.sender_slots);
        let remaining_transaction_slots =
            coordinator.remaining_transaction_slots(config.max_total_in_flight);
        let selected = enqueue_state.buffer.take_gas_with_limits(
            plan.inject_gas,
            &mut enqueue_state.sender_slots,
            remaining_transaction_slots,
        );
        let selected_gas = selected
            .iter()
            .fold(0u128, |total, tx| total.saturating_add(u128::from(tx.estimated_gas)));
        enqueue_state.offered_gas = enqueue_state.offered_gas.saturating_add(selected_gas);
        let sender_capacity_limited = selected_gas < plan.inject_gas
            && (buffered_before >= plan.inject_gas || remaining_transaction_slots == 0);
        let presign_starved = selected_gas < plan.inject_gas && !sender_capacity_limited;
        if presign_starved {
            debug!(
                requested_gas = plan.inject_gas,
                selected_gas,
                buffered_gas = enqueue_state.buffer.buffered_gas(),
                "presign buffer could not satisfy block refill"
            );
        } else if sender_capacity_limited {
            debug!(
                requested_gas = plan.inject_gas,
                selected_gas, "per-sender pool slots limited block refill"
            );
        }
        let plan_time = plan_started.elapsed();

        let submit_started = Instant::now();
        let first_batch_id = coordinator.next_batch_id();
        let batch_count = selected.len().div_ceil(SIGNED_BATCH_SIZE) as u64;
        let batches = first_batch_id..first_batch_id + batch_count;
        coordinator.ledger.open(batches.clone(), cycle_started);
        Self::enqueue_selected(enqueue_state.pipeline, coordinator, selected, true).await;
        for unsent in coordinator.next_batch_id()..batches.end {
            coordinator.ledger.abandon(unsent);
        }
        let refill_lag =
            coordinator.wait_for_refill(batches, cycle_started + REFILL_ACK_BUDGET).await;
        let submit_time = refill_lag
            .map(|lag| lag.saturating_sub(submit_started.saturating_duration_since(cycle_started)));
        let resulting_depth_gas = coordinator.tracker.unconfirmed_gas();
        let queued_gas = coordinator.queued_gas();
        coordinator.collector.record_pacing_cycle(PacingCycleObservation {
            elapsed: cycle_started
                .saturating_duration_since(config.controller.measurement_started_at),
            source: match trigger {
                RefillTrigger::Canonical(_) => PacingCycleSource::Canonical,
                RefillTrigger::Safety(_) => PacingCycleSource::Safety,
            },
            block_observed: canonical.is_some(),
            block_gas_used: canonical.map_or(0, |block| block.gas_used),
            block_gas_limit,
            our_included_gas: canonical.map_or(0, |block| block.our_included_gas),
            pre_refill_depth_gas: depth_gas,
            post_refill_depth_gas: resulting_depth_gas,
            queued_gas,
            floor_gas: plan.floor_gas,
            offered_gas: selected_gas,
            capacity_limited: matches!(plan.limited_by, InjectLimit::Capacity)
                || sender_capacity_limited,
            chain_bound: depth_gas >= plan.ceiling_gas,
            presign_starved,
            availability_lag: canonical
                .map(|block| block.observed_at.saturating_duration_since(block.expected_boundary)),
            plan_time,
            submit_time,
            refill_lag,
        });
    }

    async fn enqueue_prefill_transactions(
        enqueue_state: &mut PresignEnqueueState<'_>,
        coordinator: &mut RunCoordinator<'_>,
        target_gas: u128,
        max_in_flight_per_sender: u64,
        max_total_in_flight: usize,
        deadline: Instant,
        stop: &CancellationToken,
    ) -> Result<()> {
        let deadline = tokio::time::sleep_until(deadline.into());
        tokio::pin!(deadline);
        while enqueue_state.buffer.buffered_gas() < target_gas {
            tokio::select! {
                biased;
                () = stop.cancelled() => {
                    return Err(BaselineError::Transaction(
                        "stopped during open-loop enqueue".into(),
                    ));
                }
                () = &mut deadline => break,
                _ = coordinator.next_event() => {}
                chunk = enqueue_state.chunks.recv() => {
                    let Some(chunk) = chunk else {
                        break;
                    };
                    enqueue_state.buffer_chunk(chunk);
                }
            }
        }
        coordinator.sender_slots(max_in_flight_per_sender, &mut enqueue_state.sender_slots);
        let selected = enqueue_state.buffer.take_gas_with_limits(
            target_gas,
            &mut enqueue_state.sender_slots,
            coordinator.remaining_transaction_slots(max_total_in_flight),
        );
        Self::enqueue_selected(enqueue_state.pipeline, coordinator, selected, false).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, U256};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::{
        BlockAlignedEnqueueConfig, InjectLimit, LoadRunner, MempoolDepthController, PresignBuffer,
        PresignEnqueueState, ProducerSender,
    };
    use crate::{
        BatchRpcClient,
        config::ValiditySigningMode,
        metrics::{MetricsCollector, MetricsSummary},
        runner::{
            BlockObservation, CanonicalBlock, GasEstimate, LoadTestStage, PipelineStartConfig,
            ProgressReport, ResultsTracker, RunCoordinator, RunEvent, SubmissionPipeline,
            SubmitCohort,
        },
        workload::SelectedPayload,
    };

    /// Runs the block-aligned loop for one sender with an empty presign buffer, after queueing
    /// `events` in the coordinator inbox. Returns the summary and whether measurement finished.
    async fn run_block_aligned(
        tracker: ResultsTracker,
        config: BlockAlignedEnqueueConfig,
        events: Vec<RunEvent>,
    ) -> (MetricsSummary, bool) {
        let (events_tx, inbox) = mpsc::channel(16);
        for event in events {
            events_tx.send(event).await.expect("inbox has room");
        }
        let rpc = BatchRpcClient::new("http://127.0.0.1:1".parse().expect("valid url"))
            .expect("client builds");
        let pipeline = SubmissionPipeline::start(
            Arc::from([rpc]),
            events_tx,
            PipelineStartConfig {
                validity_signing: ValiditySigningMode::Adaptive,
                max_concurrent_submit_requests: None,
            },
        );
        let (_chunk_tx, chunks) = mpsc::channel(1);
        let (nonce_resets, _nonce_reset_rx) = mpsc::channel(1);
        let (receipt_sampler, _receipt_rx) = mpsc::channel(1);
        let (_base_fee_tx, base_fee) = watch::channel(1);
        let mut collector = MetricsCollector::new();
        let mut coordinator = RunCoordinator::new(
            inbox,
            tracker,
            &mut collector,
            receipt_sampler,
            ProgressReport {
                display: None,
                stage: LoadTestStage::Submitting,
                start: Instant::now(),
                duration: None,
                max_in_flight_per_sender: 1,
                account_count: 1,
                target_gps: None,
                base_fee,
                last_log: Instant::now(),
            },
        );
        let mut enqueue_state = PresignEnqueueState {
            pipeline: &pipeline,
            chunks,
            nonce_resets,
            buffer: PresignBuffer::new(1),
            presigned_generated: 0,
            offered_gas: 0,
            last_cycle_at: None,
            gas_estimate: GasEstimate::new(21_000),
            sender_slots: Vec::new(),
        };
        tokio::time::timeout(
            Duration::from_secs(2),
            LoadRunner::enqueue_block_aligned_transactions(
                &mut enqueue_state,
                config,
                &mut coordinator,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("enqueue loop stops on its own")
        .expect("enqueue loop exits cleanly");
        let finished = coordinator.tracker.measurement_finished();
        drop(coordinator);
        (collector.summarize(Duration::from_secs(1), None), finished)
    }

    fn block_aligned_config(
        block_time: Duration,
        deadline: Option<Instant>,
    ) -> BlockAlignedEnqueueConfig {
        BlockAlignedEnqueueConfig {
            controller: MempoolDepthController::new(
                None,
                Duration::from_secs(2),
                1_000_000,
                Instant::now(),
            ),
            fallback_block_gas_limit: 30_000_000,
            block_time,
            steady_submission_interval: None,
            presign_target_gas: 0,
            max_in_flight_per_sender: 1,
            max_total_in_flight: 1,
            deadline,
        }
    }

    fn canonical(number: u64) -> RunEvent {
        let observed_at = Instant::now();
        RunEvent::Canonical(CanonicalBlock {
            observation: BlockObservation { number, observed_at, timestamp_ms: None, base_fee: 1 },
            tx_hashes: Vec::new(),
            gas_used: 1,
            gas_limit: 30_000_000,
            expected_boundary: observed_at,
        })
    }

    #[tokio::test]
    async fn enqueue_block_aligned_stops_when_measurement_end_is_observed() {
        let mut tracker = ResultsTracker::new(&[Address::repeat_byte(0x21)]);
        tracker.begin_measurement(10, Some(2));

        let (summary, finished) = run_block_aligned(
            tracker,
            block_aligned_config(Duration::from_secs(2), None),
            vec![canonical(11), canonical(12)],
        )
        .await;

        assert_eq!(summary.pacing.canonical_cycles, 2);
        assert_eq!(summary.pacing.blocks_observed, 2);
        assert!(finished);
    }

    #[tokio::test]
    async fn enqueue_block_aligned_refills_at_safety_cadence_when_canonical_pulses_are_late() {
        let mut tracker = ResultsTracker::new(&[Address::repeat_byte(0x22)]);
        tracker.begin_measurement(10, Some(100));

        // The 200ms block interval gets a 50ms safety cadence. With no canonical pulses, this
        // performs the immediate safety refill plus periodic refills before the deadline.
        let (summary, finished) = run_block_aligned(
            tracker,
            block_aligned_config(
                Duration::from_millis(200),
                Some(Instant::now() + Duration::from_millis(210)),
            ),
            Vec::new(),
        )
        .await;

        assert_eq!(summary.pacing.canonical_cycles, 0);
        assert!(summary.pacing.safety_cycles >= 3, "{:?}", summary.pacing);
        assert!(!finished);
    }

    /// Native envelope selection and fresh-recipient values survive preparation.
    #[tokio::test]
    async fn eip8430_preparation_preserves_envelope_and_recipient() {
        let config = crate::runner::LoadConfig {
            account_count: 2,
            fresh_recipient_ratio: 1.0,
            transactions: vec![crate::runner::TxConfig {
                weight: 100,
                tx_type: crate::runner::TxType::Eip8430Transfer,
            }],
            ..crate::runner::LoadConfig::devnet()
        };
        let runner = LoadRunner::new(config.clone()).expect("native runner must initialize");
        let addresses: Vec<_> = runner.accounts.accounts().iter().map(|a| a.address).collect();
        let (_, rx) = tokio::sync::watch::channel(1);
        let (signed_chunk_tx, _) = mpsc::channel(1);
        let (_, nonce_reset_rx) = mpsc::channel(1);
        let presign = super::PresignConfig {
            senders: addresses
                .iter()
                .zip([7, 9])
                .map(|(&address, next_nonce)| ProducerSender { address, next_nonce, generation: 0 })
                .collect(),
            signers: runner.accounts.accounts().iter().map(|a| a.signer.clone()).collect(),
            chain_id: config.chain_id,
            base_fee_rx: rx,
            max_gas_price: config.max_gas_price,
            validity_priority_lead_multiplier: 1,
            validity_priority_fee_divisor: 1,
            gas_estimate: super::GasEstimate::new(50_000),
            fresh_recipient_ratio: 1.0,
            signed_chunk_tx,
            nonce_reset_rx,
            validity_router: crate::runner::ValidityRouter::new(&config),
            query_client: runner.client.clone(),
        };
        let mut generator = runner.generator;
        let mut keys = runner.recipient_keys;
        let mut rng = crate::workload::SeededRng::new(42);
        let jobs = LoadRunner::build_sender_jobs(
            &mut generator,
            None,
            &mut keys,
            &mut rng,
            &presign,
            1,
            0,
        )
        .expect("native requests must prepare");
        assert_eq!(jobs.len(), 2, "exactly one job per sender must be prepared");
        for (i, job) in jobs.iter().enumerate() {
            assert_eq!(job.start_nonce, [7, 9][i], "existing sender nonce must be retained");
            assert_eq!(job.prepared_txs.len(), 1, "exactly one request per sender is requested");
            let tx = &job.prepared_txs[0];
            assert!(tx.native, "native marker must survive request preparation");
            assert!(
                !addresses.contains(&tx.to.expect("transfer recipient")),
                "fresh recipient must not be a sender-pool address"
            );
            assert_eq!(tx.gas_limit, 200_000, "native gas ceiling must survive preparation");
            assert_eq!(
                tx.estimated_gas, 50_000,
                "pacing must use calibrated gas rather than ceiling"
            );
        }

        presign.gas_estimate.update(64 * 70_000, 64);
        let jobs = LoadRunner::build_sender_jobs(
            &mut generator,
            None,
            &mut keys,
            &mut rng,
            &presign,
            1,
            0,
        )
        .expect("native requests must prepare");
        assert_eq!(
            jobs[0].prepared_txs[0].estimated_gas, 60_000,
            "later transactions must carry the receipt-corrected estimate"
        );
    }

    /// Priority-lead validity senders send the configured lead payload with its declared gas
    /// limit; bulk validity senders stay on the weighted mix.
    #[tokio::test]
    async fn lead_payload_replaces_the_mix_for_priority_lead_senders_only() {
        let counter = Address::repeat_byte(0xcc);
        let config = crate::runner::LoadConfig {
            account_count: 32,
            transactions: vec![crate::runner::TxConfig {
                weight: 100,
                tx_type: crate::runner::TxType::DoubleCounter { contract: counter },
            }],
            validity_ratio: 1.0,
            validity_predicates: vec![crate::runner::ValidityPredicateTemplate::BlockNumber {
                op: base_execution_txpool::ValidityOperator::GreaterThanOrEqual,
                bound: crate::runner::BlockNumberBound::Absolute(U256::ZERO),
            }],
            validity_priority_lead_ratio: 0.5,
            validity_priority_lead_payload: Some(crate::runner::TxType::RevertArb {
                contract: counter,
                gas_limit: 2_000_000,
            }),
            ..crate::runner::LoadConfig::devnet()
        };
        config.validate().expect("lead payload config is valid");
        let runner = LoadRunner::new(config.clone()).expect("runner must initialize");
        let addresses: Vec<_> = runner.accounts.accounts().iter().map(|a| a.address).collect();
        let (_, rx) = tokio::sync::watch::channel(1);
        let (signed_chunk_tx, _) = mpsc::channel(1);
        let (_, nonce_reset_rx) = mpsc::channel(1);
        let presign = super::PresignConfig {
            senders: addresses
                .iter()
                .map(|&address| ProducerSender { address, next_nonce: 0, generation: 0 })
                .collect(),
            signers: runner.accounts.accounts().iter().map(|a| a.signer.clone()).collect(),
            chain_id: config.chain_id,
            base_fee_rx: rx,
            max_gas_price: config.max_gas_price,
            validity_priority_lead_multiplier: 20,
            validity_priority_fee_divisor: 1,
            gas_estimate: super::GasEstimate::new(50_000),
            fresh_recipient_ratio: 0.0,
            signed_chunk_tx,
            nonce_reset_rx,
            validity_router: crate::runner::ValidityRouter::new(&config),
            query_client: runner.client.clone(),
        };
        let lead_payload = config
            .validity_priority_lead_payload
            .as_ref()
            .map(|tx_type| SelectedPayload::from_tx_type(tx_type, None));
        let mut generator = runner.generator;
        let mut keys = runner.recipient_keys;
        let mut rng = crate::workload::SeededRng::new(42);

        let jobs = LoadRunner::build_sender_jobs(
            &mut generator,
            lead_payload.as_ref(),
            &mut keys,
            &mut rng,
            &presign,
            2,
            0,
        )
        .expect("requests must prepare");

        let mut lead_txs = 0;
        let mut bulk_txs = 0;
        for job in &jobs {
            for tx in &job.prepared_txs {
                assert_eq!(tx.to, Some(counter), "both payloads target the counter");
                assert_eq!(tx.estimated_gas, 50_000, "pacing keeps the calibrated estimate");
                assert!(!tx.validity.is_empty(), "every sender is on the validity path");
                match tx.cohort {
                    SubmitCohort::ValidityPassPriorityLead => {
                        lead_txs += 1;
                        assert_eq!(
                            tx.gas_limit, 2_000_000,
                            "reverting arb keeps its declared limit"
                        );
                        assert_eq!(tx.data.as_ref(), &crate::workload::REVERT_ARB_SELECTOR);
                    }
                    SubmitCohort::ValidityPass => {
                        bulk_txs += 1;
                        assert_eq!(tx.gas_limit, crate::workload::DOUBLE_COUNTER_GAS_LIMIT);
                        assert_eq!(
                            tx.data,
                            crate::workload::DoubleCounterPayload::encode_increment2()
                        );
                    }
                    SubmitCohort::Plain => panic!("validity_ratio 1.0 leaves no plain senders"),
                }
            }
        }
        assert!(lead_txs > 0 && bulk_txs > 0, "seeded routing must populate both cohorts");
        assert_eq!(lead_txs + bulk_txs, 64);
    }

    #[test]
    fn mempool_target_rounds_up_and_clamps_to_capacity() {
        assert_eq!(LoadRunner::mempool_target_transactions(300, 70, 10).unwrap(), 5);
        assert_eq!(LoadRunner::mempool_target_transactions(300, 70, 4).unwrap(), 4);
        assert!(LoadRunner::mempool_target_transactions(300, 0, 10).is_err());
    }
    /// The latency allowance is independent of short blocks and remains bounded.
    #[test]
    fn canonical_latency_window_is_bounded() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(375_000_000),
            Duration::from_millis(200),
            4_000_000_000,
            started,
        );
        let plan = controller.with_confirmation_latency(Duration::from_millis(1200)).plan(
            started + Duration::from_secs(2),
            150_000_000,
            0,
            0,
            0,
        );
        assert_eq!(
            plan.floor_gas, 525_000_000,
            "p99 plus one block must reserve 1.4 seconds of target gas"
        );
        assert_eq!(
            plan.ceiling_gas, 1_050_000_000,
            "catchup window must be bounded at twice the floor"
        );
        let capped = controller.with_confirmation_latency(Duration::from_secs(60)).plan(
            started + Duration::from_secs(60),
            150_000_000,
            0,
            0,
            0,
        );
        assert_eq!(
            capped.ceiling_gas, 7_500_000_000,
            "latency must cap at ten seconds before the catchup margin"
        );
        assert_eq!(capped.inject_gas, 4_000_000_000, "sender capacity remains the tighter bound");
    }

    /// Simulates delayed canonical acknowledgements with no other inclusion pulses.
    fn canonical_simulation(block_time: Duration, adaptive: bool, rpc_stall: bool) -> (u128, u128) {
        let started = Instant::now();
        let controller =
            MempoolDepthController::new(Some(375_000_000), block_time, 4_000_000_000, started);
        let mut pending = Vec::new();
        let mut offered = 0;
        let mut confirmed = 0;
        let mut peak = 0;
        let mut samples = Vec::new();
        for tick in 1..=1200u64 {
            let elapsed = Duration::from_millis(tick * 50);
            pending.retain(|&(sent, due, gas)| {
                if due <= tick {
                    confirmed += gas;
                    samples.push((tick, Duration::from_millis((tick - sent) * 50)));
                    false
                } else {
                    true
                }
            });
            let depth = offered - confirmed;
            // Advance the existing estimator's ten-second sample window on simulated time.
            samples.retain(|&(observed_tick, _)| observed_tick + 200 >= tick);
            let mut observed = crate::metrics::RollingWindow::new();
            for &(_, latency) in &samples {
                observed.push_latency(latency, Instant::now());
            }
            let latency = if adaptive { observed.p50_p99().1 } else { Duration::ZERO };
            let plan = controller.with_confirmation_latency(latency).plan(
                started + elapsed,
                1_000_000_000,
                depth,
                confirmed,
                offered,
            );
            assert!(
                plan.inject_gas <= plan.ceiling_gas.saturating_sub(depth),
                "refill must respect latency ceiling"
            );
            assert!(
                depth + plan.inject_gas <= 4_000_000_000,
                "refill must respect sender capacity"
            );
            if plan.inject_gas > 0 {
                offered += plan.inject_gas;
                // Half the submissions confirm in 700ms, with a 1.2s tail. An RPC
                // rejection retains queued work during retry backoff rather than releasing it.
                let delay = if rpc_stall && tick >= 400 {
                    1200
                } else if tick % 100 >= 98 {
                    24
                } else if tick % 100 < 50 {
                    14
                } else {
                    18
                };
                // Batch inclusion into actual block intervals before observation delay.
                let block_ticks =
                    u64::try_from(block_time.as_millis() / 50).expect("block ticks fit u64");
                let due = tick.div_ceil(block_ticks) * block_ticks + delay;
                pending.push((tick, due, plan.inject_gas));
            }
            peak = peak.max(offered - confirmed);
        }
        (offered, peak)
    }

    /// Canonical-only feedback sustains short-block targets and bounds RPC retry backlog.
    #[test]
    fn canonical_only_pacing_simulation() {
        let before = canonical_simulation(Duration::from_millis(200), false, false);
        let after = canonical_simulation(Duration::from_millis(200), true, false);
        let stalled = canonical_simulation(Duration::from_millis(200), true, true);
        let legacy = canonical_simulation(Duration::from_secs(2), true, false);
        assert_eq!(
            before,
            (9_975_000_000, 150_000_000),
            "old window must reproduce the exact canonical-only inventory bottleneck"
        );
        assert_eq!(
            after,
            (22_500_000_000, 468_750_000),
            "latency window must sustain 375M gas/s with bounded peak inventory"
        );
        assert_eq!(
            stalled,
            (8_606_250_000, 1_125_000_000),
            "stalled feedback must stop offers at the bounded inventory window"
        );
        assert_eq!(
            legacy,
            (22_500_000_000, 1_050_000_000),
            "2s canonical batching must retain full target rate and bounded inventory"
        );
    }

    #[test]
    fn controller_limits_refills_to_cumulative_rate_budget() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(1_000_000),
            Duration::from_secs(2),
            10_000_000,
            started,
        );

        let initial = controller.plan(started, 30_000_000, 0, 0, 0);
        assert_eq!(initial.floor_gas, 2_000_000);
        assert_eq!(initial.inject_gas, 0);
        assert_eq!(initial.limited_by, InjectLimit::Rate);

        let behind = controller.plan(started + Duration::from_secs(2), 30_000_000, 0, 0, 0);
        assert_eq!(behind.desired_gas, 4_000_000);
        assert_eq!(behind.inject_gas, 2_000_000);
        assert_eq!(behind.limited_by, InjectLimit::Rate);

        let budget_spent =
            controller.plan(started + Duration::from_secs(2), 30_000_000, 0, 0, 2_000_000);
        assert_eq!(budget_spent.inject_gas, 0);
        assert_eq!(budget_spent.limited_by, InjectLimit::Rate);
    }

    #[test]
    fn controller_reports_capacity_limit_without_error() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(1_000_000),
            Duration::from_secs(2),
            1_000_000,
            started,
        );

        let plan = controller.plan(started + Duration::from_secs(2), 30_000_000, 0, 0, 0);

        assert_eq!(plan.inject_gas, 1_000_000);
        assert_eq!(plan.limited_by, InjectLimit::Capacity);
    }

    #[test]
    fn controller_does_not_inject_when_depth_meets_floor_on_target() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(1_000_000),
            Duration::from_secs(2),
            10_000_000,
            started,
        );

        let plan = controller.plan(
            started + Duration::from_secs(2),
            30_000_000,
            2_000_000,
            2_000_000,
            2_000_000,
        );

        assert_eq!(plan.desired_gas, 2_000_000);
        assert_eq!(plan.inject_gas, 0);
        assert_eq!(plan.limited_by, InjectLimit::Nothing);
    }

    #[test]
    fn steady_controller_sends_the_rate_budget_regardless_of_depth() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(1_000_000),
            Duration::from_secs(2),
            10_000_000,
            started,
        )
        .with_steady_submission(Some(Duration::from_secs(1)));

        // Depth already meets the 2M floor, which stops a block-aligned refill (see the test
        // above). Steady submission still sends the 1M due: 2M of target minus 1M offered.
        let plan = controller.plan(
            started + Duration::from_secs(2),
            30_000_000,
            2_000_000,
            2_000_000,
            1_000_000,
        );

        assert_eq!(plan.desired_gas, plan.ceiling_gas);
        assert_eq!(plan.inject_gas, 1_000_000);
        assert_eq!(plan.limited_by, InjectLimit::Rate);
    }

    #[test]
    fn steady_controller_drains_a_backlog_without_bursting() {
        let started = Instant::now();
        let interval = Duration::from_millis(20);
        let controller = MempoolDepthController::new(
            Some(400_000_000),
            Duration::from_millis(200),
            u128::MAX,
            started,
        )
        .with_steady_submission(Some(interval));

        // Ten seconds behind (4 Ggas owed) with an empty pool, 20 ms after the previous cycle:
        // block-aligned catch-up would send up to the 160M ceiling at once. Steady sends twice
        // the 8M due over those 20 ms: 16M.
        let now = started + Duration::from_secs(10);
        let plan =
            controller.with_previous_cycle(Some(now - interval)).plan(now, 160_000_000, 0, 0, 0);

        assert_eq!(plan.inject_gas, 16_000_000);
        assert_eq!(plan.limited_by, InjectLimit::Rate);

        // A cycle 2 ms after the previous one, such as a canonical pulse right after a tick,
        // gets only its own share.
        let plan = controller.with_previous_cycle(Some(now - Duration::from_millis(2))).plan(
            now,
            160_000_000,
            0,
            0,
            0,
        );
        assert_eq!(plan.inject_gas, 1_600_000);
    }

    #[test]
    fn steady_controller_lets_a_late_cycle_catch_up() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(400_000_000),
            Duration::from_millis(200),
            u128::MAX,
            started,
        )
        .with_steady_submission(Some(Duration::from_millis(20)));

        // The previous cycle ran 100 ms ago and everything due until then was sent, so 40M is
        // due now. A per-cycle cap of two 20 ms intervals would send only 16M and lose rate.
        let now = started + Duration::from_secs(10);
        let plan = controller.with_previous_cycle(Some(now - Duration::from_millis(100))).plan(
            now,
            160_000_000,
            0,
            0,
            3_960_000_000,
        );

        assert_eq!(plan.inject_gas, 40_000_000);
    }

    #[test]
    fn controller_cannot_inject_past_two_block_ceiling() {
        let started = Instant::now();
        let controller = MempoolDepthController::new(
            Some(1_000_000),
            Duration::from_secs(2),
            10_000_000,
            started,
        );

        let plan = controller.plan(
            started + Duration::from_secs(20),
            30_000_000,
            4_000_000,
            0,
            20_000_000,
        );

        assert_eq!(plan.ceiling_gas, 4_000_000);
        assert_eq!(plan.inject_gas, 0);
    }

    #[test]
    fn unbounded_controller_targets_two_full_blocks() {
        let started = Instant::now();
        let controller =
            MempoolDepthController::new(None, Duration::from_secs(2), 100_000_000, started);

        let plan = controller.plan(started, 30_000_000, 0, 0, 0);

        assert_eq!(plan.floor_gas, 30_000_000);
        assert_eq!(plan.desired_gas, 60_000_000);
        assert_eq!(plan.inject_gas, 60_000_000);
    }

    #[tokio::test]
    async fn run_does_not_start_after_an_earlier_stop() {
        let mut runner =
            LoadRunner::new(crate::runner::LoadConfig::devnet()).expect("runner must initialize");
        runner.stop_token().cancel();

        let error = runner.run().await.expect_err("a stopped runner must not start a run");
        assert!(error.to_string().contains("stopped before the load test started"));
    }
}
