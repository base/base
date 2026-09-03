//! Inventory headroom, presign streaming, and measured enqueue pacing.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_network::Ethereum;
use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_tx_manager::NonceManager;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::{
    sync::{mpsc, watch},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{
    BlockWatcher, DisplaySnapshot, FlashblockWatcher, GasPricer, InclusionPulse, InclusionSource,
    LoadRunner, LoadTestDisplay, LoadTestStage, PipelineStartConfig, PreparedTransaction,
    PresignBuffer, QueuedSubmitFailures, ResultsTracker, SignedBatch, SignedTransaction,
    SubmissionPipeline, SubmitEvent, TxType, ValidityRouter,
};
use crate::{
    BaselineError, Result,
    metrics::{MetricsCollector, MetricsSummary, PacingCycleObservation, PacingCycleSource},
    rpc::{BaseFeeExt, QueryProvider},
    workload::{KeyStream, SeededRng, WorkloadGenerator},
};

const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const SUBMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMIT_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const PENDING_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(200);
const CONFIRMATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(200);
/// How long the post-run drain waits for progress (a confirmation or a shrinking
/// pending count) before force-expiring whatever is left and returning early.
///
/// Without this, a straggler submitted near the end of the run effectively gets
/// its own full `PENDING_CONFIRMATION_TIMEOUT` window measured from `drain_start`
/// (not from when it was submitted), because `drain_expiry` only shrinks to zero
/// at `drain_start + PENDING_CONFIRMATION_TIMEOUT`. Stuck inventory can therefore
/// hold up the whole drain for ~200s even on a short test.
const DRAIN_STALL_TIMEOUT: Duration = Duration::from_secs(20);
const SIGNED_BATCH_SIZE: usize = 200;
const PRESIGN_CHANNEL_BUFFER: usize = 2;
const IDLE_SLEEP: Duration = Duration::from_millis(10);
const PRESIGN_RECHECK_INTERVAL: Duration = Duration::from_millis(100);
const PREFILL_TIMEOUT: Duration = Duration::from_secs(300);
/// Adaptive open-loop target update cadence, matching closed-loop rate-limiter updates.
/// TUI / snapshot refresh cadence while the enqueue loop owns the collector.
const DISPLAY_RENDER_INTERVAL: Duration = Duration::from_millis(500);
/// Structured progress reporting cadence for long-running stages.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct SenderJob {
    sender_index: usize,
    generation: u64,
    from: Address,
    start_nonce: u64,
    prepared_txs: Vec<PreparedTransaction>,
}

#[derive(Debug)]
struct SignedSender {
    sender_index: usize,
    generation: u64,
    signed_txs: Vec<SignedTransaction>,
}

#[derive(Debug)]
struct PresignProducerState {
    generator: WorkloadGenerator,
    recipient_keys: Option<KeyStream>,
    recipient_rng: SeededRng,
}

struct PresignConfig {
    sender_addresses: Vec<Address>,
    sender_next_nonces: Vec<u64>,
    /// Generation paired with `sender_next_nonces`; bumped when submission reports a terminal
    /// rejection so stale signed chunks cannot be enqueued after a nonce resync.
    sender_generations: Vec<u64>,
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    nonce_managers: Arc<HashMap<Address, NonceManager<RootProvider<Ethereum>>>>,
    chain_id: u64,
    base_fee_rx: watch::Receiver<u128>,
    max_gas_price: u128,
    validity_priority_lead_multiplier: u128,
    validity_priority_fee_divisor: u128,
    estimated_gas: u64,
    fresh_recipient_ratio: f64,
    signed_chunk_tx: mpsc::Sender<Vec<PresignedSenderBatch>>,
    nonce_reset_rx: mpsc::Receiver<SenderNonceReset>,
    validity_router: ValidityRouter,
    /// Query provider used to read the current block height per prepare round
    /// when resolving offset-based `block_number` validity predicates.
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
    address: Address,
    /// New generation that the producer must adopt after refetching the sender's pending nonce.
    generation: u64,
}

struct EnqueueProgress {
    presigned_generated: u64,
    offered_gas: u128,
}

/// Live TUI / snapshot updates while the enqueue loop owns the collector.
struct EnqueueProgressDisplay<'a> {
    display: Option<&'a LoadTestDisplay>,
    snapshot_tx: Option<&'a watch::Sender<DisplaySnapshot>>,
    last_update: Instant,
    last_log: Instant,
    start: Instant,
    duration: Option<Duration>,
    stage: LoadTestStage,
    max_in_flight_per_sender: usize,
    account_count: usize,
    gas_price_gwei: f64,
    target_gps: Option<u64>,
}

impl EnqueueProgressDisplay<'_> {
    fn maybe_refresh(
        &mut self,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        let should_render = (self.display.is_some() || self.snapshot_tx.is_some())
            && self.last_update.elapsed() >= DISPLAY_RENDER_INTERVAL;
        let should_log = self.last_log.elapsed() >= PROGRESS_REPORT_INTERVAL;
        if !should_render && !should_log {
            return;
        }
        let elapsed = self.start.elapsed();
        collector.sample_throughput(elapsed);

        let (p50, p99) = collector.rolling_p50_p99();
        let (flashblocks_p50, flashblocks_p99) = collector.rolling_flashblocks_p50_p99();
        if should_log {
            info!(
                stage = self.stage.as_str(),
                elapsed_secs = elapsed.as_secs(),
                remaining_secs =
                    self.duration.map(|duration| duration.saturating_sub(elapsed).as_secs()),
                submitted = collector.submitted_count(),
                confirmed = collector.confirmed_count(),
                failed = collector.failed_count(),
                in_flight = results_tracker.total_in_flight(),
                rolling_gps = collector.rolling_gps(),
                "load test progress"
            );
            self.last_log = Instant::now();
        }
        if !should_render {
            return;
        }
        self.last_update = Instant::now();
        let snap = DisplaySnapshot {
            elapsed,
            duration: self.duration,
            stage: self.stage,
            submitted: collector.submitted_count(),
            confirmed: collector.confirmed_count(),
            failed: collector.failed_count(),
            in_flight: results_tracker.total_in_flight(),
            senders_blocked: results_tracker.senders_at_limit(self.max_in_flight_per_sender as u64),
            total_senders: self.account_count,
            rolling_tps: collector.rolling_tps(),
            rolling_gps: collector.rolling_gps(),
            target_gps: self.target_gps,
            p50_latency: p50,
            p99_latency: p99,
            flashblocks_p50_latency: flashblocks_p50,
            flashblocks_p99_latency: flashblocks_p99,
            gas_price_gwei: self.gas_price_gwei,
        };
        if let Some(display) = self.display {
            display.update(&snap);
        }
        if let Some(tx) = self.snapshot_tx {
            let _ = tx.send(snap);
        }
    }
}

struct EnqueueDrainState<'a> {
    submit_event_rx: &'a mut mpsc::Receiver<SubmitEvent>,
    queued_per_sender: &'a mut HashMap<Address, u64>,
    rejected_senders: &'a mut HashSet<Address>,
    queued_gas: &'a mut u128,
    collector: &'a mut MetricsCollector,
    results_tracker: &'a ResultsTracker,
    progress_display: Option<EnqueueProgressDisplay<'a>>,
}

struct PresignEnqueueState<'a> {
    submission_pipeline: &'a SubmissionPipeline,
    next_submit_batch_id: &'a AtomicU64,
    signed_chunk_rx: &'a mut mpsc::Receiver<Vec<PresignedSenderBatch>>,
    nonce_reset_tx: &'a mpsc::Sender<SenderNonceReset>,
    sender_indices: &'a HashMap<Address, usize>,
    buffer: &'a mut PresignBuffer,
    progress: &'a mut EnqueueProgress,
    base_fee_tx: &'a watch::Sender<u128>,
}

#[derive(Debug, Clone, Copy)]
struct BlockAlignedEnqueueConfig {
    controller: MempoolDepthController,
    fallback_block_gas_limit: u64,
    block_time: Duration,
    presign_target_gas: u128,
    max_in_flight_per_sender: usize,
    max_total_in_flight: usize,
    deadline: Option<Instant>,
}

impl EnqueueDrainState<'_> {
    fn apply_submit_event(&mut self, event: SubmitEvent) {
        LoadRunner::apply_submit_event(
            event,
            self.queued_per_sender,
            self.rejected_senders,
            self.queued_gas,
            self.collector,
            self.results_tracker,
        );
        self.record_completed_refills();
    }

    fn drain_run_events(&mut self) {
        LoadRunner::drain_run_events(
            self.submit_event_rx,
            self.queued_per_sender,
            self.rejected_senders,
            self.queued_gas,
            self.collector,
            self.results_tracker,
        );
        self.record_completed_refills();
        if let Some(progress) = self.progress_display.as_mut() {
            progress.maybe_refresh(self.collector, self.results_tracker);
        }
    }

    fn record_completed_refills(&mut self) {
        for lag in self.results_tracker.drain_completed_refill_lags() {
            self.collector.record_completed_refill_lag(lag);
        }
    }

    fn mempool_depth_gas(&self) -> u128 {
        // Local submission backlog has not reached the node yet. Counting it as
        // mempool inventory can starve the RPC pipeline while acknowledgements lag.
        self.results_tracker.unconfirmed_gas()
    }

    fn remaining_transaction_slots(&self, capacity: usize) -> usize {
        let queued =
            self.queued_per_sender.values().fold(0u64, |total, count| total.saturating_add(*count));
        let occupied = self.results_tracker.total_in_flight().saturating_add(queued);
        capacity.saturating_sub(usize::try_from(occupied).unwrap_or(usize::MAX))
    }
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
    /// One-block gas floor.
    pub floor_gas: u128,
    /// Two-block gas ceiling.
    pub ceiling_gas: u128,
    /// Constraint that determined the plan.
    pub limited_by: InjectLimit,
}

/// Pure controller that keeps one to two blocks of gas in the mempool.
#[derive(Debug, Clone, Copy)]
pub struct MempoolDepthController {
    target_gps: Option<u64>,
    block_time: Duration,
    capacity_gas: u128,
    measurement_started_at: Instant,
}

impl MempoolDepthController {
    /// Creates a controller for the measured run.
    pub const fn new(
        target_gps: Option<u64>,
        block_time: Duration,
        capacity_gas: u128,
        measurement_started_at: Instant,
    ) -> Self {
        Self { target_gps, block_time, capacity_gas, measurement_started_at }
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
        let ceiling_gas = floor_gas.saturating_mul(2);
        let catchup = self.target_gps.map_or(floor_gas, |target_gps| {
            let cumulative_target = u128::from(target_gps)
                .saturating_mul(
                    now.saturating_duration_since(self.measurement_started_at).as_nanos(),
                )
                .div_ceil(Duration::from_secs(1).as_nanos());
            cumulative_target.saturating_sub(confirmed_gas).min(floor_gas)
        });
        let desired_gas = floor_gas.saturating_add(catchup).min(ceiling_gas);
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
        if self.b20_run_salt.is_none()
            && self.config.transactions.iter().any(|t| matches!(t.tx_type, TxType::B20))
        {
            return Err(BaselineError::Config(
                "b20 run salt not set; call prepare_payloads before run".into(),
            ));
        }

        self.collector.reset();
        self.stop_flag.store(false, Ordering::SeqCst);
        self.cancel_token = CancellationToken::new();

        self.base_fee = self.client.get_base_fee().await?;
        info!(base_fee = self.base_fee, "fetched current base fee");

        if !self.validity_router.is_disabled() {
            self.probe_validity_endpoint().await?;
        }

        for account in self.accounts.accounts() {
            if !self.nonce_managers.contains_key(&account.address) {
                let provider = RootProvider::<Ethereum>::new_http(self.config.query_rpc.clone());
                let nonce_manager = NonceManager::new(provider, account.address, NONCE_RPC_TIMEOUT)
                    .with_pending_tag();
                Arc::make_mut(&mut self.nonce_managers).insert(account.address, nonce_manager);
            }
        }

        const SUBMIT_CHANNEL_BUFFER: usize = 32_768;
        let (submit_event_tx, mut submit_event_rx) =
            mpsc::channel::<SubmitEvent>(SUBMIT_CHANNEL_BUFFER);

        let sender_addresses: Vec<_> = self.accounts.accounts().iter().map(|a| a.address).collect();
        const INCLUSION_PULSE_BUFFER: usize = 1_024;
        let (inclusion_pulse_tx, mut inclusion_pulse_rx) =
            mpsc::channel::<InclusionPulse>(INCLUSION_PULSE_BUFFER);
        let results_tracker =
            ResultsTracker::new_with_pulse_sender(&sender_addresses, inclusion_pulse_tx.clone());

        let receipt_provider = RootProvider::<Base>::new_http(self.config.query_rpc.clone());
        let watcher_cancel = self.cancel_token.child_token();
        let _watcher_cancel_guard = watcher_cancel.clone().drop_guard();
        let block_watcher_task = Some(
            BlockWatcher::new(
                receipt_provider.clone(),
                results_tracker.clone(),
                self.config.block_time,
                inclusion_pulse_tx.clone(),
                watcher_cancel.clone(),
            )
            .start(),
        );
        let flashblock_watcher_task = self.config.flashblocks_ws.clone().map(|ws_url| {
            FlashblockWatcher::new(
                ws_url,
                results_tracker.clone(),
                inclusion_pulse_tx.clone(),
                watcher_cancel.clone(),
            )
            .start()
        });
        drop(inclusion_pulse_tx);

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let initial_avg_gas = self.calibrate_avg_gas().await?;
        // Seed the collector so live throughput (rolling GPS) and rate-limiter
        // feedback have a non-zero gas figure before canonical receipt gas lands.
        self.collector.set_estimated_gas(initial_avg_gas);
        let mut start = Instant::now();
        let account_count = self.accounts.len();

        info!(
            sender_count = account_count,
            signer_worker_count = SubmissionPipeline::signer_worker_count(
                self.submission_batch_rpcs.len(),
                self.config.max_concurrent_submit_requests,
            ),
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

        let signers = Arc::clone(&self.signers);
        let nonce_managers = Arc::clone(&self.nonce_managers);
        let submission_batch_rpcs = Arc::clone(&self.submission_batch_rpcs);
        let mut submission_pipeline = SubmissionPipeline::start(
            signers,
            nonce_managers,
            submission_batch_rpcs,
            results_tracker.clone(),
            submit_event_tx.clone(),
            PipelineStartConfig {
                chain_id: self.config.chain_id,
                max_gas_price: self.config.max_gas_price,
                validity_priority_lead_multiplier: self.config.validity_priority_lead_multiplier,
                validity_priority_fee_divisor: self.config.validity_priority_fee_divisor,
                max_concurrent_submit_requests: self.config.max_concurrent_submit_requests,
            },
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let mut queued_per_sender: HashMap<Address, u64> =
            self.accounts.accounts().iter().map(|a| (a.address, 0)).collect();
        let mut rejected_senders = HashSet::new();
        let mut queued_gas = 0u128;

        let mut last_base_fee_refresh = Instant::now();
        // Refresh once per block so the cached base fee tracks the climb the load
        // test itself induces; a stale fee mints underwater (unincludable) txs.
        const BASE_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

        let use_live_display = self.display.as_ref().is_some_and(|d| d.is_active());
        let use_snapshot_tx = self.snapshot_tx.is_some();

        // Emit an initial snapshot immediately so the TUI renders live
        // metrics (submitted/in-flight/failed counters) without waiting
        // for the first confirmation to arrive.
        if use_live_display || use_snapshot_tx {
            let snap = self.build_snapshot(
                start,
                &results_tracker,
                max_in_flight_per_sender,
                account_count,
                LoadTestStage::Submitting,
            );
            if let Some(ref d) = self.display {
                d.update(&snap);
            }
            if let Some(ref tx) = self.snapshot_tx {
                let _ = tx.send(snap);
            }
        }

        let mut open_loop_enqueue_error: Option<BaselineError> = None;
        let pre_sign_started = Instant::now();
        let sender_addresses: Vec<Address> =
            self.accounts.accounts().iter().map(|account| account.address).collect();
        let sender_indices: HashMap<Address, usize> = sender_addresses
            .iter()
            .copied()
            .enumerate()
            .map(|(index, address)| (address, index))
            .collect();
        let nonce_initialization_started = Instant::now();
        let sender_start_nonces = self.sender_start_nonces(&sender_addresses).await?;
        let sender_count = sender_addresses.len();
        info!(
            sender_count,
            elapsed_ms = nonce_initialization_started.elapsed().as_millis(),
            "sender nonces initialized"
        );

        let capacity = self.config.effective_in_flight_capacity(account_count);
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
        let target = Self::mempool_target_transactions(
            u64::try_from(floor_gas).unwrap_or(u64::MAX),
            initial_avg_gas,
            capacity as u64,
        )?;
        let initial_target_in_flight = target;

        let replacement_generator = WorkloadGenerator::from_tx_configs(
            self.workload_config(),
            &self.config.transactions,
            self.b20_run_salt,
        )?;
        let producer_generator = std::mem::replace(&mut self.generator, replacement_generator);
        let producer_recipient_keys = self.recipient_keys.take();
        let producer_recipient_rng = std::mem::take(&mut self.recipient_rng);

        let (signed_chunk_tx, mut signed_chunk_rx) = mpsc::channel(PRESIGN_CHANNEL_BUFFER);
        let (nonce_reset_tx, nonce_reset_rx) = mpsc::channel(sender_count.max(1));
        let (base_fee_tx, base_fee_rx) = watch::channel(self.base_fee);
        let pacing_base_fee_tx = base_fee_tx.clone();
        let base_fee_client = self.client.clone();
        let base_fee_cancel = self.cancel_token.clone();
        let base_fee_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = base_fee_cancel.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(1)) => {
                        if let Ok(base_fee) = base_fee_client.get_base_fee().await {
                            base_fee_tx.send_replace(base_fee);
                        }
                    }
                }
            }
        });
        let producer_task = tokio::spawn(Self::stream_presigned_transactions(
            PresignProducerState {
                generator: producer_generator,
                recipient_keys: producer_recipient_keys,
                recipient_rng: producer_recipient_rng,
            },
            PresignConfig {
                sender_addresses,
                sender_next_nonces: sender_start_nonces,
                sender_generations: vec![0; sender_count],
                signers: Arc::clone(&self.signers),
                nonce_managers: Arc::clone(&self.nonce_managers),
                chain_id: self.config.chain_id,
                base_fee_rx,
                max_gas_price: self.config.max_gas_price,
                validity_priority_lead_multiplier: self.config.validity_priority_lead_multiplier,
                validity_priority_fee_divisor: self.config.validity_priority_fee_divisor,
                estimated_gas: initial_avg_gas,
                fresh_recipient_ratio: self.config.fresh_recipient_ratio,
                signed_chunk_tx,
                nonce_reset_rx,
                validity_router: self.validity_router.clone(),
                query_client: self.client.clone(),
            },
        ));

        let mut progress = EnqueueProgress { presigned_generated: 0, offered_gas: 0 };
        let mut presign_buffer = PresignBuffer::new(sender_count);

        if let Some(display) = self.display.as_ref() {
            display.set_stage(LoadTestStage::Prefill);
        }
        let prefill_deadline = Instant::now() + PREFILL_TIMEOUT;
        let prefill_result = Self::enqueue_prefill_transactions(
            &mut PresignEnqueueState {
                submission_pipeline: &submission_pipeline,
                next_submit_batch_id: &next_submit_batch_id,
                signed_chunk_rx: &mut signed_chunk_rx,
                nonce_reset_tx: &nonce_reset_tx,
                sender_indices: &sender_indices,
                buffer: &mut presign_buffer,
                progress: &mut progress,
                base_fee_tx: &pacing_base_fee_tx,
            },
            floor_gas.min(u128::from(capacity as u64).saturating_mul(u128::from(initial_avg_gas))),
            max_in_flight_per_sender,
            capacity,
            prefill_deadline,
            &self.stop_flag,
            &mut EnqueueDrainState {
                submit_event_rx: &mut submit_event_rx,
                queued_per_sender: &mut queued_per_sender,
                rejected_senders: &mut rejected_senders,
                queued_gas: &mut queued_gas,
                collector: &mut self.collector,
                results_tracker: &results_tracker,
                progress_display: Some(EnqueueProgressDisplay {
                    display: self.display.as_ref(),
                    snapshot_tx: self.snapshot_tx.as_ref(),
                    last_update: Instant::now()
                        .checked_sub(DISPLAY_RENDER_INTERVAL)
                        .unwrap_or_else(Instant::now),
                    last_log: Instant::now(),
                    start,
                    duration: None,
                    stage: LoadTestStage::Prefill,
                    max_in_flight_per_sender,
                    account_count,
                    gas_price_gwei: self.base_fee as f64 / 1e9,
                    target_gps: self.config.target_gps,
                }),
            },
        )
        .await;

        if prefill_result.is_ok() {
            let drain_started = Instant::now();
            while submission_pipeline.pending_batches() > 0
                && drain_started.elapsed() < SUBMIT_DRAIN_TIMEOUT
            {
                Self::drain_run_events(
                    &mut submit_event_rx,
                    &mut queued_per_sender,
                    &mut rejected_senders,
                    &mut queued_gas,
                    &mut self.collector,
                    &results_tracker,
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut rejected_senders,
                &mut queued_gas,
                &mut self.collector,
                &results_tracker,
            );
            let pending_batches = submission_pipeline.pending_batches();
            if pending_batches > 0 {
                warn!(
                    pending_batches,
                    "warmup submission pipeline is still draining; continuing as a measured bottleneck"
                );
            }
        }

        if prefill_result.is_ok() {
            let ready_file = self.config.separate_setup.as_deref().map(|dir| dir.join("ready"));
            Self::publish_handshake(ready_file.as_deref())?;
            self.wait_for_start_file().await?;
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut rejected_senders,
                &mut queued_gas,
                &mut self.collector,
                &results_tracker,
            );
            while inclusion_pulse_rx.try_recv().is_ok() {}
            let measurement_start_block =
                receipt_provider.get_block_number().await.map_err(|e| {
                    BaselineError::Rpc(format!(
                        "failed to read latest block number for measurement start: {e}"
                    ))
                })?;
            results_tracker
                .begin_measurement(measurement_start_block, self.config.measurement_blocks);
            self.collector.reset();
            self.collector.set_estimated_gas(initial_avg_gas);
            start = Instant::now();
            let started_file = self.config.separate_setup.as_deref().map(|dir| dir.join("started"));
            Self::publish_handshake(started_file.as_deref())?;
        }

        let enqueue_deadline = self.config.duration.map(|d| start + d);
        let enqueue_result = if let Err(err) = prefill_result {
            Err(err)
        } else {
            if let Some(display) = self.display.as_ref() {
                display.set_stage(LoadTestStage::Submitting);
            }
            info!(
                initial_target_in_flight,
                duration_secs = ?self.config.duration.map(|d| d.as_secs()),
                "starting open-loop measured enqueue"
            );
            let depth_controller = MempoolDepthController::new(
                self.config.target_gps,
                self.config.block_time,
                u128::from(capacity as u64).saturating_mul(u128::from(initial_avg_gas)),
                start,
            );
            Self::enqueue_block_aligned_transactions(
                &mut PresignEnqueueState {
                    submission_pipeline: &submission_pipeline,
                    next_submit_batch_id: &next_submit_batch_id,
                    signed_chunk_rx: &mut signed_chunk_rx,
                    nonce_reset_tx: &nonce_reset_tx,
                    sender_indices: &sender_indices,
                    buffer: &mut presign_buffer,
                    progress: &mut progress,
                    base_fee_tx: &pacing_base_fee_tx,
                },
                BlockAlignedEnqueueConfig {
                    controller: depth_controller,
                    fallback_block_gas_limit: block_gas_limit,
                    block_time: self.config.block_time,
                    presign_target_gas: floor_gas.saturating_mul(2).min(
                        u128::from(capacity as u64).saturating_mul(u128::from(initial_avg_gas)),
                    ),
                    max_in_flight_per_sender,
                    max_total_in_flight: capacity,
                    deadline: enqueue_deadline,
                },
                &mut inclusion_pulse_rx,
                &self.stop_flag,
                &mut EnqueueDrainState {
                    submit_event_rx: &mut submit_event_rx,
                    queued_per_sender: &mut queued_per_sender,
                    rejected_senders: &mut rejected_senders,
                    queued_gas: &mut queued_gas,
                    collector: &mut self.collector,
                    results_tracker: &results_tracker,
                    progress_display: Some(EnqueueProgressDisplay {
                        display: self.display.as_ref(),
                        snapshot_tx: self.snapshot_tx.as_ref(),
                        last_update: Instant::now()
                            .checked_sub(DISPLAY_RENDER_INTERVAL)
                            .unwrap_or_else(Instant::now),
                        last_log: Instant::now(),
                        start,
                        duration: self.config.duration,
                        stage: LoadTestStage::Submitting,
                        max_in_flight_per_sender,
                        account_count,
                        gas_price_gwei: self.base_fee as f64 / 1e9,
                        target_gps: self.config.target_gps,
                    }),
                },
            )
            .await
        };
        self.collector.set_pacing_duration(start.elapsed());

        let finished_file = self.config.separate_setup.as_deref().map(|dir| dir.join("finished"));
        Self::publish_handshake(finished_file.as_deref())?;

        drop(signed_chunk_rx);

        match producer_task.await {
            Ok(Ok(producer_state)) => {
                self.generator = producer_state.generator;
                self.recipient_keys = producer_state.recipient_keys;
                self.recipient_rng = producer_state.recipient_rng;
            }
            Ok(Err(err)) => {
                warn!(error = %err, "open-loop pre-sign producer failed");
                if open_loop_enqueue_error.is_none() {
                    open_loop_enqueue_error = Some(err);
                }
            }
            Err(join_error) => {
                let err = BaselineError::Transaction(format!(
                    "open-loop pre-sign producer task failed: {join_error}"
                ));
                warn!(error = %err, "open-loop pre-sign producer task failed");
                if open_loop_enqueue_error.is_none() {
                    open_loop_enqueue_error = Some(err);
                }
            }
        }
        base_fee_task.abort();

        if let Err(err) = enqueue_result {
            warn!(
                error = %err,
                presigned_generated = progress.presigned_generated,
                "open-loop enqueue aborted; proceeding to drain and shutdown"
            );
            open_loop_enqueue_error = Some(err);
        } else {
            info!(
                submitted = self.collector.submitted_count(),
                offered_gas = progress.offered_gas,
                elapsed_secs = pre_sign_started.elapsed().as_secs_f64(),
                "load test enqueue complete"
            );
        }

        while self.config.duration.is_none_or(|d| start.elapsed() < d)
            && !self.stop_flag.load(Ordering::SeqCst)
            && open_loop_enqueue_error.is_none()
        {
            // --- Housekeeping (runs once per batch iteration) ---

            if last_base_fee_refresh.elapsed() >= BASE_FEE_REFRESH_INTERVAL {
                if let Ok(new_base_fee) = self.client.get_base_fee().await
                    && new_base_fee != self.base_fee
                {
                    debug!(old_base_fee = self.base_fee, new_base_fee, "base fee updated");
                    self.base_fee = new_base_fee;
                }
                last_base_fee_refresh = Instant::now();
            }

            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut rejected_senders,
                &mut queued_gas,
                &mut self.collector,
                &results_tracker,
            );

            // Drain flashblock observations for the rolling window (separate from
            // confirmed metrics to avoid double-counting in the final summary).
            for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
                self.collector.record_flashblock_observed(latency, observed_at);
            }
            // Drain confirmed metrics non-blocking so the rolling window stays
            // current during the run (not just during the post-run drain).
            for metrics in results_tracker.drain_confirmed_metrics() {
                self.collector.record_confirmed(metrics);
            }
            let expired = results_tracker.expire_pending(PENDING_CONFIRMATION_TIMEOUT);
            if expired > 0 {
                self.collector.record_failures("expired without confirmation", expired);
            }

            tokio::time::sleep(IDLE_SLEEP).await;
        }

        submission_pipeline.close_input();

        let drain_started = Instant::now();
        while submission_pipeline.pending_batches() > 0
            && drain_started.elapsed() < SUBMIT_DRAIN_TIMEOUT
        {
            Self::drain_submit_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut rejected_senders,
                &mut queued_gas,
                &mut self.collector,
                &results_tracker,
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let pending_submit_batches = submission_pipeline.pending_batches();
        if pending_submit_batches > 0 {
            warn!(
                pending_submit_batches,
                "timed out waiting for submit queue to drain, closing submit queue"
            );
            let failures =
                submission_pipeline.close_and_fail_queued("submit queue abandoned").await;
            Self::apply_queued_submit_failures(
                failures,
                &mut queued_per_sender,
                &mut self.collector,
            );
        }
        submission_pipeline.shutdown_and_join(SUBMIT_WORKER_SHUTDOWN_TIMEOUT).await;
        drop(submission_pipeline);

        // Close the channel so the drain below cannot miss late events.
        drop(submit_event_tx);

        Self::drain_submit_events(
            &mut submit_event_rx,
            &mut queued_per_sender,
            &mut rejected_senders,
            &mut queued_gas,
            &mut self.collector,
            &results_tracker,
        );
        for lag in results_tracker.drain_completed_refill_lags() {
            self.collector.record_completed_refill_lag(lag);
        }

        // Keep background watchers alive through the drain so late flashblock
        // inclusions and block observations can still be joined into metrics.
        self.stop_flag.store(true, Ordering::SeqCst);

        self.set_display_stage(LoadTestStage::DrainingConfirmations);

        let submitted = self.collector.submitted_count();
        let in_flight = results_tracker.total_in_flight();
        let elapsed = start.elapsed();
        info!(
            submitted,
            in_flight,
            elapsed_secs = elapsed.as_secs(),
            actual_tps = submitted as f64 / elapsed.as_secs_f64(),
            "load test complete, draining confirmations"
        );

        let drain_start = Instant::now();
        let confirmation_drain_timeout = if open_loop_enqueue_error.is_some() {
            warn!("skipping confirmation drain after open-loop enqueue failure");
            Duration::ZERO
        } else {
            CONFIRMATION_DRAIN_TIMEOUT
        };
        let results_poll_interval = Duration::from_millis(600);
        let mut last_confirmed_at = start.elapsed();
        let mut last_pending_count = results_tracker.pending_count();
        let mut last_drain_progress = Instant::now();
        let mut last_drain_report = Instant::now();

        while drain_start.elapsed() < confirmation_drain_timeout {
            for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
                self.collector.record_flashblock_observed(latency, observed_at);
            }
            let metrics = results_tracker.drain_confirmed_metrics();
            let has_confirmed = !metrics.is_empty();
            if has_confirmed {
                last_confirmed_at = start.elapsed();
                for metrics in metrics {
                    self.collector.record_confirmed(metrics);
                }
            }
            if self.display.as_ref().is_some_and(LoadTestDisplay::is_active)
                || self.snapshot_tx.is_some()
            {
                let snap = self.build_snapshot(
                    start,
                    &results_tracker,
                    max_in_flight_per_sender,
                    account_count,
                    LoadTestStage::DrainingConfirmations,
                );
                if let Some(display) = &self.display {
                    display.update(&snap);
                }
                if let Some(snapshot_tx) = &self.snapshot_tx {
                    let _ = snapshot_tx.send(snap);
                }
            }

            let pending_count = results_tracker.pending_count();
            if pending_count == 0 {
                break;
            }
            if pending_count < last_pending_count || has_confirmed {
                last_pending_count = pending_count;
                last_drain_progress = Instant::now();
            } else if last_drain_progress.elapsed() >= DRAIN_STALL_TIMEOUT {
                // No confirmations and no shrinkage for a while: the remainder is
                // expected mempool inventory at the measurement cutoff. Remove it
                // from tracking without classifying it as a failed transaction.
                let (stalled, undrained_gas) = results_tracker.measured_unconfirmed_inventory();
                results_tracker.expire_pending(Duration::ZERO);
                self.collector.record_undrained_inventory(stalled, undrained_gas);
                info!(
                    stalled,
                    undrained_gas,
                    stall_secs = DRAIN_STALL_TIMEOUT.as_secs(),
                    "confirmation drain stopped with expected undrained inventory"
                );
                break;
            }
            if last_drain_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
                info!(
                    stage = LoadTestStage::DrainingConfirmations.as_str(),
                    elapsed_secs = drain_start.elapsed().as_secs(),
                    remaining_secs = Option::<u64>::None,
                    submitted = self.collector.submitted_count(),
                    confirmed = self.collector.confirmed_count(),
                    failed = self.collector.failed_count(),
                    in_flight = results_tracker.total_in_flight(),
                    rolling_gps = self.collector.rolling_gps(),
                    "load test progress"
                );
                last_drain_report = Instant::now();
            }

            tokio::time::sleep(results_poll_interval).await;
        }

        if results_tracker.pending_count() > 0 {
            let (undrained, undrained_gas) = results_tracker.measured_unconfirmed_inventory();
            results_tracker.expire_pending(Duration::ZERO);
            self.collector.record_undrained_inventory(undrained, undrained_gas);
        }

        for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
            self.collector.record_flashblock_observed(latency, observed_at);
        }
        for metrics in results_tracker.drain_confirmed_metrics() {
            self.collector.record_confirmed(metrics);
            last_confirmed_at = start.elapsed();
        }

        // Now safe to stop background watcher tasks.
        self.cancel_token.cancel();

        if let Some(task) = block_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "block watcher panicked"),
                _ => {}
            }
        }
        if let Some(task) = flashblock_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(error)) if error.is_panic() => {
                    warn!(error = %error, "flashblock watcher panicked");
                }
                _ => {}
            }
        }

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        // Fetch canonical receipts in a single batch pass, scoped to only the blocks
        // our transactions landed in, to backfill gas and revert status. This can be
        // slow on large runs, so notify the user before starting.
        let landed_blocks = results_tracker.landed_block_numbers();
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
            self.collector.apply_receipts(&receipts_by_hash, landed_blocks.len(), failed_blocks);
            info!(
                blocks = landed_blocks.len(),
                failed_blocks,
                receipts = receipts_by_hash.len(),
                elapsed_secs = receipt_fetch_start.elapsed().as_secs_f64(),
                "end-of-run receipt pass complete"
            );
        }

        let mut summary = self.collector.summarize_with_fresh_recipient_count(
            last_confirmed_at,
            self.config_summary.clone(),
            self.fresh_recipient_count(),
        );
        let measurement_window = results_tracker.measurement_window();
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

    async fn sender_start_nonces(&self, sender_addresses: &[Address]) -> Result<Vec<u64>> {
        let nonce_futures = sender_addresses.iter().map(|from| async move {
            let nonce_manager = self.nonce_managers.get(from).ok_or_else(|| {
                BaselineError::Transaction(format!("missing nonce manager for sender {from}"))
            })?;

            nonce_manager.reset().await;
            let nonce_guard = nonce_manager.next_nonce().await.map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to fetch starting nonce for sender {from}: {e}"
                ))
            })?;
            let nonce = nonce_guard.nonce();
            nonce_guard.rollback();
            Ok(nonce)
        });

        stream::iter(nonce_futures)
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

    fn build_sender_jobs(
        generator: &mut WorkloadGenerator,
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        config: &PresignConfig,
        txs_per_sender: usize,
        current_block: u64,
    ) -> Result<Vec<SenderJob>> {
        if config.sender_addresses.len() != config.sender_next_nonces.len() {
            return Err(BaselineError::Transaction(format!(
                "open-loop sender nonce set mismatch: {} addresses vs {} nonces",
                config.sender_addresses.len(),
                config.sender_next_nonces.len(),
            )));
        }
        if config.sender_addresses.len() != config.sender_generations.len() {
            return Err(BaselineError::Transaction(format!(
                "open-loop sender generation set mismatch: {} addresses vs {} generations",
                config.sender_addresses.len(),
                config.sender_generations.len(),
            )));
        }

        let sender_count = config.sender_addresses.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }
        let mut sender_jobs = Vec::with_capacity(sender_count);
        for (sender_index, from) in config.sender_addresses.iter().copied().enumerate() {
            let sender_pool_recipient = config.sender_addresses[(sender_index + 1) % sender_count];
            let cohort = config.validity_router.cohort_for_sender(from);
            let start_nonce = config.sender_next_nonces[sender_index];
            let mut prepared_txs = Vec::with_capacity(txs_per_sender);
            for nonce_offset in 0..txs_per_sender {
                let nonce_offset = u64::try_from(nonce_offset).map_err(|error| {
                    BaselineError::Transaction(format!(
                        "failed to convert nonce offset to u64: {error}"
                    ))
                })?;
                let nonce = start_nonce.checked_add(nonce_offset).ok_or_else(|| {
                    BaselineError::Transaction(format!(
                        "nonce overflow for sender {from} at offset {nonce_offset}"
                    ))
                })?;
                let payload = generator.select_payload()?;
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
                let validity = config.validity_router.predicates_for(
                    cohort,
                    current_block,
                    nonce,
                    from,
                    to_addr,
                );

                prepared_txs.push(PreparedTransaction {
                    from,
                    to: to_addr,
                    value,
                    data,
                    gas_limit,
                    estimated_gas: config.estimated_gas,
                    validity,
                    cohort,
                });
            }

            sender_jobs.push(SenderJob {
                sender_index,
                from,
                start_nonce,
                generation: config.sender_generations[sender_index],
                prepared_txs,
            });
        }

        Ok(sender_jobs)
    }

    async fn sign_sender_jobs(
        sender_jobs: Vec<SenderJob>,
        signers: Arc<HashMap<Address, PrivateKeySigner>>,
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
            let jobs = sender_jobs
                .by_ref()
                .take(jobs_per_task)
                .map(|sender_job| {
                    let signer = signers.get(&sender_job.from).cloned().ok_or_else(|| {
                        BaselineError::Transaction(format!(
                            "missing signer for sender {}",
                            sender_job.from
                        ))
                    })?;
                    Ok((sender_job, signer))
                })
                .collect::<Result<Vec<_>>>()?;
            if jobs.is_empty() {
                break;
            }
            signing_tasks.push(task::spawn_blocking(move || {
                jobs.into_iter()
                    .map(|(sender_job, signer)| {
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

        let mut signed_by_sender: Vec<Option<SignedSender>> =
            std::iter::repeat_with(|| None).take(sender_count).collect();

        for signing_task in signing_tasks {
            let signed_senders = signing_task.await.map_err(|e| {
                BaselineError::Transaction(format!("open-loop signing task failed: {e}"))
            })??;
            for signed_sender in signed_senders {
                let sender_index = signed_sender.sender_index;
                if signed_by_sender[sender_index].is_some() {
                    return Err(BaselineError::Transaction(format!(
                        "duplicate signed sender result for index {sender_index}"
                    )));
                }
                signed_by_sender[sender_index] = Some(signed_sender);
            }
        }

        let mut ordered_signed_txs = Vec::with_capacity(sender_count);
        for (sender_index, signed_sender) in signed_by_sender.into_iter().enumerate() {
            let signed_sender = signed_sender.ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "missing signed transaction set for sender index {sender_index}"
                ))
            })?;
            ordered_signed_txs.push(PresignedSenderBatch {
                sender_index,
                generation: signed_sender.generation,
                txs: signed_sender.signed_txs,
            });
        }

        Ok(ordered_signed_txs)
    }

    async fn stream_presigned_transactions(
        mut producer_state: PresignProducerState,
        mut config: PresignConfig,
    ) -> Result<PresignProducerState> {
        if config.sender_addresses.is_empty() {
            return Ok(producer_state);
        }

        let sender_count = config.sender_addresses.len();
        let chunk_per_sender = (SIGNED_BATCH_SIZE / sender_count).max(1);

        // The producer streams indefinitely and only stops when the consumer drops
        // `signed_chunk_rx` (detected below via the `send(...).is_err()` check), which
        // happens once the enqueue loop returns (deadline reached or channel closed).
        loop {
            // Terminal rejections create a local nonce gap for already-presigned work. Drain reset
            // requests before building the next chunk so affected senders resume from the node's
            // pending nonce instead of being removed from the active sender set.
            Self::drain_nonce_reset_requests(&mut config).await?;

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
                &mut producer_state.recipient_keys,
                &mut producer_state.recipient_rng,
                &config,
                chunk_per_sender,
                current_block,
            )?;

            let base_fee = *config.base_fee_rx.borrow_and_update();
            let signed_by_sender = Self::sign_sender_jobs(
                sender_jobs,
                Arc::clone(&config.signers),
                config.chain_id,
                base_fee,
                config.max_gas_price,
                config.validity_priority_lead_multiplier,
                config.validity_priority_fee_divisor,
            )
            .await?;

            if config.signed_chunk_tx.send(signed_by_sender).await.is_err() {
                break;
            }

            let nonce_increment = u64::try_from(chunk_per_sender).map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to convert open-loop chunk size to nonce increment: {e}"
                ))
            })?;
            for (sender_index, next_nonce) in config.sender_next_nonces.iter_mut().enumerate() {
                *next_nonce = next_nonce.checked_add(nonce_increment).ok_or_else(|| {
                    BaselineError::Transaction(format!(
                        "nonce overflow while advancing open-loop producer sender index {sender_index}"
                    ))
                })?;
            }
        }

        Ok(producer_state)
    }

    async fn drain_nonce_reset_requests(config: &mut PresignConfig) -> Result<()> {
        while let Ok(reset) = config.nonce_reset_rx.try_recv() {
            let Some(address) = config.sender_addresses.get(reset.sender_index).copied() else {
                continue;
            };
            if address != reset.address {
                continue;
            }
            if config
                .sender_generations
                .get(reset.sender_index)
                .is_some_and(|generation| *generation >= reset.generation)
            {
                continue;
            }

            let nonce_manager = config.nonce_managers.get(&reset.address).ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "missing nonce manager for sender {} during nonce reset",
                    reset.address
                ))
            })?;
            // The nonce manager uses the pending tag for load-test senders, so this restarts after
            // transactions the node already accepted while avoiding the rejected stale tail.
            nonce_manager.reset().await;
            let nonce_guard = nonce_manager.next_nonce().await.map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to reset nonce for sender {}: {e}",
                    reset.address
                ))
            })?;
            let next_nonce = nonce_guard.nonce();
            nonce_guard.rollback();

            config.sender_next_nonces[reset.sender_index] = next_nonce;
            config.sender_generations[reset.sender_index] = reset.generation;
            info!(
                sender = %reset.address,
                sender_index = reset.sender_index,
                generation = reset.generation,
                next_nonce,
                "resynced open-loop sender after terminal rejection"
            );
        }

        Ok(())
    }

    fn sign_sender_job(
        sender_job: SenderJob,
        signer: PrivateKeySigner,
        chain_id: u64,
        base_fee: u128,
        max_gas_price: u128,
        validity_priority_lead_multiplier: u128,
        validity_priority_fee_divisor: u128,
    ) -> Result<SignedSender> {
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

            let fees = GasPricer::new(max_gas_price).fees_for_cohort(
                base_fee,
                prepared.cohort,
                validity_priority_fee_divisor,
                validity_priority_lead_multiplier,
            );
            signed_txs.push(SubmissionPipeline::sign_at_nonce(
                &signer, &prepared, chain_id, nonce, fees,
            )?);
        }

        Ok(SignedSender {
            sender_index: sender_job.sender_index,
            generation: sender_job.generation,
            signed_txs,
        })
    }

    async fn enqueue_signed_while_draining(
        submission_pipeline: &SubmissionPipeline,
        batch: SignedBatch,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> std::result::Result<(), SignedBatch> {
        let enqueue = submission_pipeline.enqueue_signed(batch);
        tokio::pin!(enqueue);

        loop {
            tokio::select! {
                result = enqueue.as_mut() => {
                    drain_state.drain_run_events();
                    return result;
                }
                maybe_event = drain_state.submit_event_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            drain_state.apply_submit_event(event);
                            drain_state.drain_run_events();
                        }
                        None => return enqueue.as_mut().await,
                    }
                }
            }
        }
    }

    async fn enqueue_block_aligned_transactions(
        enqueue_state: &mut PresignEnqueueState<'_>,
        config: BlockAlignedEnqueueConfig,
        inclusion_pulse_rx: &mut mpsc::Receiver<InclusionPulse>,
        stop_flag: &AtomicBool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Result<()> {
        let safety_interval =
            (config.block_time / 4).clamp(Duration::from_millis(1), Duration::from_millis(250));
        let mut safety_tick = tokio::time::interval(safety_interval);
        safety_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_block_gas_limit = config.fallback_block_gas_limit;
        let signal_timeout =
            config.block_time + (config.block_time / 2).min(Duration::from_millis(250));
        let mut last_pulse_at =
            Instant::now().checked_sub(signal_timeout).unwrap_or_else(Instant::now);

        loop {
            drain_state.drain_run_events();
            if stop_flag.load(Ordering::SeqCst)
                || config.deadline.is_some_and(|deadline| Instant::now() >= deadline)
                || drain_state.results_tracker.measurement_finished()
            {
                return Ok(());
            }

            tokio::select! {
                biased;
                maybe_pulse = inclusion_pulse_rx.recv() => {
                    let Some(pulse) = maybe_pulse else {
                        return Ok(());
                    };
                    if let Some(canonical) = pulse.canonical {
                        last_block_gas_limit = canonical.gas_limit;
                        enqueue_state.base_fee_tx.send_replace(canonical.base_fee);
                    }
                    last_pulse_at = pulse.observed_at;
                    if drain_state.results_tracker.measurement_finished() {
                        return Ok(());
                    }
                    Self::run_refill_cycle(
                        enqueue_state,
                        pulse,
                        last_block_gas_limit,
                        &config,
                        drain_state,
                    )
                    .await?;
                }
                _ = safety_tick.tick() => {
                    if last_pulse_at.elapsed() >= signal_timeout {
                        Self::run_refill_cycle(
                            enqueue_state,
                            InclusionPulse::safety(Instant::now()),
                            last_block_gas_limit,
                            &config,
                            drain_state,
                        )
                        .await?;
                        last_pulse_at = Instant::now();
                    }
                }
                maybe_chunk = enqueue_state.signed_chunk_rx.recv(),
                    if enqueue_state.buffer.buffered_gas() < config.presign_target_gas => {
                    let Some(chunk) = maybe_chunk else {
                        return Ok(());
                    };
                    Self::buffer_presigned_chunk(
                        enqueue_state.buffer,
                        enqueue_state.progress,
                        chunk,
                    );
                }
            }
        }
    }

    fn buffer_presigned_chunk(
        buffer: &mut PresignBuffer,
        progress: &mut EnqueueProgress,
        chunk: Vec<PresignedSenderBatch>,
    ) {
        for sender_batch in chunk {
            progress.presigned_generated =
                progress.presigned_generated.saturating_add(sender_batch.txs.len() as u64);
            buffer.push_sender_batch(
                sender_batch.sender_index,
                sender_batch.generation,
                sender_batch.txs,
            );
        }
    }

    async fn run_refill_cycle(
        enqueue_state: &mut PresignEnqueueState<'_>,
        pulse: InclusionPulse,
        fallback_block_gas_limit: u64,
        config: &BlockAlignedEnqueueConfig,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Result<()> {
        let cycle_started = pulse.observed_at;
        let canonical = pulse.canonical;
        while enqueue_state.buffer.buffered_gas() < config.presign_target_gas {
            let Ok(chunk) = enqueue_state.signed_chunk_rx.try_recv() else {
                break;
            };
            Self::buffer_presigned_chunk(enqueue_state.buffer, enqueue_state.progress, chunk);
        }
        drain_state.drain_run_events();
        let mut reset_sender_count = 0usize;
        for sender in drain_state.rejected_senders.drain() {
            let Some(&sender_index) = enqueue_state.sender_indices.get(&sender) else {
                continue;
            };
            let Some(generation) = enqueue_state.buffer.reset_sender(sender_index, sender) else {
                continue;
            };
            // Reset is split across the enqueue and producer tasks: the enqueue side owns the
            // presign buffer and generation filter, while the producer owns future nonce
            // assignment. The control message stitches those two local state machines together.
            if enqueue_state
                .nonce_reset_tx
                .send(SenderNonceReset { sender_index, address: sender, generation })
                .await
                .is_err()
            {
                warn!(
                    sender = %sender,
                    sender_index,
                    generation,
                    "presign producer stopped before sender nonce reset"
                );
                continue;
            }
            reset_sender_count = reset_sender_count.saturating_add(1);
        }
        if reset_sender_count > 0 {
            warn!(reset_sender_count, "reset senders after terminal nonce rejection");
        }

        let plan_started = Instant::now();
        let depth_gas = drain_state.mempool_depth_gas();
        let block_gas_limit = canonical.map_or(fallback_block_gas_limit, |block| block.gas_limit);
        let plan = config.controller.plan(
            cycle_started,
            block_gas_limit,
            depth_gas,
            drain_state.results_tracker.confirmed_gas(),
            enqueue_state.progress.offered_gas,
        );
        let buffered_before = enqueue_state.buffer.buffered_gas();
        let mut sender_slots: HashMap<Address, u64> = drain_state
            .queued_per_sender
            .iter()
            .map(|(from, queued)| {
                let occupied =
                    queued.saturating_add(drain_state.results_tracker.in_flight_for(from));
                (
                    *from,
                    u64::try_from(config.max_in_flight_per_sender)
                        .unwrap_or(u64::MAX)
                        .saturating_sub(occupied),
                )
            })
            .collect();
        let remaining_transaction_slots =
            drain_state.remaining_transaction_slots(config.max_total_in_flight);
        let mut selected = enqueue_state.buffer.take_gas_with_limits(
            plan.inject_gas,
            &mut sender_slots,
            remaining_transaction_slots,
        );
        let selected_gas = selected
            .iter()
            .fold(0u128, |total, tx| total.saturating_add(u128::from(tx.estimated_gas)));
        enqueue_state.progress.offered_gas =
            enqueue_state.progress.offered_gas.saturating_add(selected_gas);
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
        let first_batch_id = enqueue_state.next_submit_batch_id.load(Ordering::SeqCst);
        while !selected.is_empty() {
            if !Self::enqueue_signed_batch(
                enqueue_state.submission_pipeline,
                enqueue_state.next_submit_batch_id,
                &mut selected,
                SIGNED_BATCH_SIZE,
                true,
                drain_state,
            )
            .await
            {
                break;
            }
        }
        let batch_ids: Vec<u64> =
            (first_batch_id..enqueue_state.next_submit_batch_id.load(Ordering::SeqCst)).collect();
        let refill_lag = Self::wait_for_batch_completions(
            &batch_ids,
            cycle_started,
            Duration::from_millis(100),
            drain_state,
        )
        .await;
        let submit_time = refill_lag
            .map(|lag| lag.saturating_sub(submit_started.saturating_duration_since(cycle_started)));
        if !batch_ids.is_empty() && refill_lag.is_none() {
            drain_state.results_tracker.register_pending_refill(batch_ids.clone(), cycle_started);
        }
        let resulting_depth_gas = drain_state.mempool_depth_gas();
        drain_state.collector.record_pacing_cycle(PacingCycleObservation {
            elapsed: cycle_started
                .saturating_duration_since(config.controller.measurement_started_at),
            source: match pulse.source {
                InclusionSource::Canonical => PacingCycleSource::Canonical,
                InclusionSource::Flashblock => PacingCycleSource::Flashblock,
                InclusionSource::Safety => PacingCycleSource::Safety,
            },
            block_observed: canonical.is_some(),
            block_gas_used: canonical.map_or(0, |block| block.gas_used),
            block_gas_limit,
            our_included_gas: canonical.map_or(0, |block| block.our_included_gas),
            pre_refill_depth_gas: depth_gas,
            post_refill_depth_gas: resulting_depth_gas,
            queued_gas: *drain_state.queued_gas,
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
        Ok(())
    }

    async fn wait_for_batch_completions(
        batch_ids: &[u64],
        cycle_started: Instant,
        budget: Duration,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Option<Duration> {
        if batch_ids.is_empty() {
            return Some(Duration::ZERO);
        }

        let deadline = cycle_started + budget;
        loop {
            drain_state.drain_run_events();
            if let Some(completed_at) =
                drain_state.results_tracker.take_completed_batches(batch_ids)
            {
                return Some(completed_at.saturating_duration_since(cycle_started));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, drain_state.submit_event_rx.recv()).await {
                Ok(Some(event)) => drain_state.apply_submit_event(event),
                Ok(None) | Err(_) => return None,
            }
        }
    }

    async fn enqueue_prefill_transactions(
        enqueue_state: &mut PresignEnqueueState<'_>,
        target_gas: u128,
        max_in_flight_per_sender: usize,
        max_total_in_flight: usize,
        deadline: Instant,
        stop_flag: &AtomicBool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Result<()> {
        while enqueue_state.buffer.buffered_gas() < target_gas {
            drain_state.drain_run_events();
            if stop_flag.load(Ordering::SeqCst) {
                return Err(BaselineError::Transaction("stopped during open-loop enqueue".into()));
            }
            if Instant::now() >= deadline {
                break;
            }
            let chunk = match tokio::time::timeout(
                PRESIGN_RECHECK_INTERVAL,
                enqueue_state.signed_chunk_rx.recv(),
            )
            .await
            {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => continue,
            };
            Self::buffer_presigned_chunk(enqueue_state.buffer, enqueue_state.progress, chunk);
        }

        let mut sender_slots: HashMap<Address, u64> = drain_state
            .queued_per_sender
            .keys()
            .map(|from| (*from, u64::try_from(max_in_flight_per_sender).unwrap_or(u64::MAX)))
            .collect();
        let mut selected = enqueue_state.buffer.take_gas_with_limits(
            target_gas,
            &mut sender_slots,
            drain_state.remaining_transaction_slots(max_total_in_flight),
        );
        while !selected.is_empty() {
            if !Self::enqueue_signed_batch(
                enqueue_state.submission_pipeline,
                enqueue_state.next_submit_batch_id,
                &mut selected,
                SIGNED_BATCH_SIZE,
                false,
                drain_state,
            )
            .await
            {
                break;
            }
        }

        Ok(())
    }

    async fn enqueue_signed_batch(
        submission_pipeline: &SubmissionPipeline,
        next_submit_batch_id: &AtomicU64,
        pending_signed_batch: &mut Vec<SignedTransaction>,
        max_batch_len: usize,
        measured: bool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> bool {
        let batch_len = pending_signed_batch.len().min(max_batch_len);
        let signed_txs: Vec<SignedTransaction> = pending_signed_batch.drain(..batch_len).collect();
        let batch_id = next_submit_batch_id.fetch_add(1, Ordering::SeqCst);

        for signed_tx in &signed_txs {
            drain_state
                .queued_per_sender
                .entry(signed_tx.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            *drain_state.queued_gas =
                drain_state.queued_gas.saturating_add(u128::from(signed_tx.estimated_gas));
        }

        let batch = SignedBatch { id: batch_id, attempt: 0, measured, txs: signed_txs };
        match Self::enqueue_signed_while_draining(submission_pipeline, batch, drain_state).await {
            Ok(()) => true,
            Err(batch) => {
                warn!(
                    batch_id,
                    batch_len, "submit queue closed while enqueuing open-loop signed batch"
                );
                let failed_count = batch.txs.len() as u64;
                for signed_tx in &batch.txs {
                    if let Some(count) = drain_state.queued_per_sender.get_mut(&signed_tx.from) {
                        *count = count.saturating_sub(1);
                    }
                    *drain_state.queued_gas =
                        drain_state.queued_gas.saturating_sub(u128::from(signed_tx.estimated_gas));
                }
                drain_state.collector.record_failures("submit queue closed", failed_count);
                false
            }
        }
    }

    fn build_snapshot(
        &mut self,
        start: Instant,
        results_tracker: &ResultsTracker,
        max_in_flight_per_sender: usize,
        account_count: usize,
        stage: LoadTestStage,
    ) -> DisplaySnapshot {
        let (p50, p99) = self.collector.rolling_p50_p99();
        let (flashblocks_p50, flashblocks_p99) = self.collector.rolling_flashblocks_p50_p99();
        DisplaySnapshot {
            elapsed: start.elapsed(),
            duration: self.config.duration,
            stage,
            submitted: self.collector.submitted_count(),
            confirmed: self.collector.confirmed_count(),
            failed: self.collector.failed_count(),
            in_flight: results_tracker.total_in_flight(),
            senders_blocked: results_tracker.senders_at_limit(max_in_flight_per_sender as u64),
            total_senders: account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            target_gps: self.config.target_gps,
            p50_latency: p50,
            p99_latency: p99,
            flashblocks_p50_latency: flashblocks_p50,
            flashblocks_p99_latency: flashblocks_p99,
            gas_price_gwei: self.base_fee as f64 / 1e9,
        }
    }

    fn apply_submit_event(
        event: SubmitEvent,
        queued_per_sender: &mut HashMap<Address, u64>,
        rejected_senders: &mut HashSet<Address>,
        queued_gas: &mut u128,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        match event {
            SubmitEvent::Submitted(tx_hash) => collector.record_submitted(tx_hash),
            SubmitEvent::Failed(reason) => {
                collector.record_failed(TxHash::ZERO, &reason);
            }
            SubmitEvent::Released { from, estimated_gas, accepted } => {
                if let Some(count) = queued_per_sender.get_mut(&from) {
                    *count = count.saturating_sub(1);
                }
                *queued_gas = queued_gas.saturating_sub(u128::from(estimated_gas));
                if !accepted {
                    rejected_senders.insert(from);
                }
            }
            SubmitEvent::BatchCompleted { id, completed_at } => {
                results_tracker.record_batch_completed(id, completed_at);
            }
        }
    }

    fn drain_submit_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        rejected_senders: &mut HashSet<Address>,
        queued_gas: &mut u128,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(
                event,
                queued_per_sender,
                rejected_senders,
                queued_gas,
                collector,
                results_tracker,
            );
        }
    }

    fn drain_run_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        rejected_senders: &mut HashSet<Address>,
        queued_gas: &mut u128,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(
                event,
                queued_per_sender,
                rejected_senders,
                queued_gas,
                collector,
                results_tracker,
            );
        }
        for metrics in results_tracker.drain_confirmed_metrics() {
            collector.record_confirmed(metrics);
        }
        for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
            collector.record_flashblock_observed(latency, observed_at);
        }
    }

    fn apply_queued_submit_failures(
        failures: QueuedSubmitFailures,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        for (from, released) in failures.released_by_sender {
            if let Some(count) = queued_per_sender.get_mut(&from) {
                *count = count.saturating_sub(released);
            }
        }
        if failures.failed_count > 0 {
            collector.record_failures(failures.reason, failures.failed_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64},
        },
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, Bytes, TxHash};
    use tokio::sync::mpsc;

    use super::{
        BlockAlignedEnqueueConfig, EnqueueDrainState, EnqueueProgress, InclusionPulse, InjectLimit,
        LoadRunner, MempoolDepthController, PresignBuffer, PresignEnqueueState,
    };
    use crate::{
        metrics::MetricsCollector,
        runner::{
            BlockObservation, BlockPulse, PipelineStartConfig, ResultsTracker,
            SUBMIT_BATCH_QUEUE_BUFFER, SignedBatch, SignedTransaction, SubmissionPipeline,
            SubmitCohort, SubmitEvent,
        },
    };
    #[test]
    fn mempool_target_rounds_up_and_clamps_to_capacity() {
        assert_eq!(LoadRunner::mempool_target_transactions(300, 70, 10).unwrap(), 5);
        assert_eq!(LoadRunner::mempool_target_transactions(300, 70, 4).unwrap(), 4);
        assert!(LoadRunner::mempool_target_transactions(300, 0, 10).is_err());
    }

    fn test_signed_batch(id: u64, from: Address) -> SignedBatch {
        SignedBatch {
            id,
            attempt: 0,
            measured: true,
            txs: vec![SignedTransaction {
                raw: Bytes::new(),
                tx_hash: TxHash::repeat_byte((id % 0xff) as u8),
                from,
                nonce: id,
                gas_limit: 21_000,
                estimated_gas: 21_000,
                validity: Vec::new(),
                cohort: SubmitCohort::Plain,
            }],
        }
    }

    #[tokio::test]
    async fn enqueue_signed_while_draining_makes_progress_when_enqueue_is_backpressured() {
        let sender = Address::repeat_byte(0x11);
        let tracked_senders = vec![sender];
        let results_tracker = ResultsTracker::new(&tracked_senders);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::from([(sender, 1_u64)]);
        let mut rejected_senders = HashSet::new();
        let mut queued_gas = 21_000u128;

        let (submit_event_tx, mut submit_event_rx) = mpsc::channel(1);
        let mut submission_pipeline = SubmissionPipeline::start(
            Arc::new(HashMap::new()),
            Arc::new(HashMap::new()),
            Arc::new(Vec::new()),
            results_tracker.clone(),
            submit_event_tx.clone(),
            PipelineStartConfig {
                chain_id: 1,
                max_gas_price: u128::MAX,
                validity_priority_lead_multiplier: 1,
                validity_priority_fee_divisor: 1,
                max_concurrent_submit_requests: None,
            },
        );

        submission_pipeline.shutdown_and_join(Duration::from_secs(1)).await;

        for batch_id in 0..SUBMIT_BATCH_QUEUE_BUFFER {
            submission_pipeline
                .enqueue_signed(test_signed_batch(batch_id as u64, sender))
                .await
                .expect("signed queue should accept up to capacity");
        }

        submit_event_tx
            .send(SubmitEvent::Released { from: sender, estimated_gas: 21_000, accepted: true })
            .await
            .expect("event queue should accept first event");

        let release_after_event_drain = async {
            submit_event_tx
                .send(SubmitEvent::Submitted(TxHash::repeat_byte(0xaa)))
                .await
                .expect("second send should unblock only after event drain");
            let _ = submission_pipeline.close_and_fail_queued("test queue close").await;
        };

        let (enqueue_result, ()) = {
            let mut drain_state = EnqueueDrainState {
                submit_event_rx: &mut submit_event_rx,
                queued_per_sender: &mut queued_per_sender,
                rejected_senders: &mut rejected_senders,
                queued_gas: &mut queued_gas,
                collector: &mut collector,
                results_tracker: &results_tracker,

                progress_display: None,
            };
            let enqueue_attempt = LoadRunner::enqueue_signed_while_draining(
                &submission_pipeline,
                test_signed_batch((SUBMIT_BATCH_QUEUE_BUFFER + 1) as u64, sender),
                &mut drain_state,
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(enqueue_attempt, release_after_event_drain)
            })
            .await
            .expect("enqueue should complete once event drain unblocks it")
        };

        assert!(enqueue_result.is_err(), "enqueue should fail after queue is closed");
        assert_eq!(queued_per_sender.get(&sender).copied(), Some(0));
        assert_eq!(collector.submitted_count(), 1);
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
    async fn enqueue_block_aligned_stops_when_measurement_end_is_observed() {
        let sender = Address::repeat_byte(0x21);
        let results_tracker = ResultsTracker::new(&[sender]);
        results_tracker.begin_measurement(10, Some(2));
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::from([(sender, 0_u64)]);
        let mut rejected_senders = HashSet::new();
        let mut queued_gas = 0u128;
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel(8);
        let (_signed_chunk_tx, mut signed_chunk_rx) = mpsc::channel(1);
        let (base_fee_tx, _base_fee_rx) = tokio::sync::watch::channel(1_u128);
        let mut progress = EnqueueProgress { presigned_generated: 0, offered_gas: 0 };
        let mut presign_buffer = PresignBuffer::new(1);
        let pipeline = SubmissionPipeline::start(
            Arc::new(HashMap::new()),
            Arc::new(HashMap::new()),
            Arc::new(Vec::new()),
            results_tracker.clone(),
            mpsc::channel(1).0,
            PipelineStartConfig {
                chain_id: 1,
                max_gas_price: u128::MAX,
                validity_priority_lead_multiplier: 1,
                validity_priority_fee_divisor: 1,
                max_concurrent_submit_requests: None,
            },
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let stop_flag = AtomicBool::new(false);
        let (pulse_tx, mut pulse_rx) = mpsc::channel(8);
        let (nonce_reset_tx, _nonce_reset_rx) = mpsc::channel(1);
        let sender_indices = HashMap::from([(sender, 0usize)]);

        let mut enqueue_state = PresignEnqueueState {
            submission_pipeline: &pipeline,
            next_submit_batch_id: &next_submit_batch_id,
            signed_chunk_rx: &mut signed_chunk_rx,
            nonce_reset_tx: &nonce_reset_tx,
            sender_indices: &sender_indices,
            buffer: &mut presign_buffer,
            progress: &mut progress,
            base_fee_tx: &base_fee_tx,
        };
        let mut drain_state = EnqueueDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            rejected_senders: &mut rejected_senders,
            queued_gas: &mut queued_gas,
            collector: &mut collector,
            results_tracker: &results_tracker,
            progress_display: None,
        };

        let (run_result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                LoadRunner::enqueue_block_aligned_transactions(
                    &mut enqueue_state,
                    BlockAlignedEnqueueConfig {
                        controller: MempoolDepthController::new(
                            None,
                            Duration::from_secs(2),
                            1_000_000,
                            Instant::now(),
                        ),
                        fallback_block_gas_limit: 30_000_000,
                        block_time: Duration::from_secs(2),
                        presign_target_gas: 0,
                        max_in_flight_per_sender: 1,
                        max_total_in_flight: 1,
                        deadline: None,
                    },
                    &mut pulse_rx,
                    &stop_flag,
                    &mut drain_state,
                ),
                async {
                    let first_observed_at = Instant::now();
                    results_tracker.on_new_block_hashes(
                        BlockObservation { number: 11, observed_at: first_observed_at },
                        Vec::new(),
                    );
                    pulse_tx
                        .send(InclusionPulse::canonical(
                            BlockPulse {
                                number: 11,
                                gas_used: 1,
                                gas_limit: 30_000_000,
                                base_fee: 1,
                                our_included_gas: 0,
                                expected_boundary: first_observed_at,
                                observed_at: first_observed_at,
                            },
                            0,
                        ))
                        .await
                        .expect("first pulse should send");
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;

                    let second_observed_at = Instant::now();
                    results_tracker.on_new_block_hashes(
                        BlockObservation { number: 12, observed_at: second_observed_at },
                        Vec::new(),
                    );
                    pulse_tx
                        .send(InclusionPulse::canonical(
                            BlockPulse {
                                number: 12,
                                gas_used: 1,
                                gas_limit: 30_000_000,
                                base_fee: 1,
                                our_included_gas: 0,
                                expected_boundary: second_observed_at,
                                observed_at: second_observed_at,
                            },
                            0,
                        ))
                        .await
                        .expect("second pulse should send");
                }
            )
        })
        .await
        .expect("enqueue loop should stop at measurement end");
        run_result.expect("enqueue loop should exit cleanly");

        let summary = collector.summarize(Duration::from_secs(1), None);
        assert_eq!(summary.pacing.canonical_cycles, 1);
        assert!(results_tracker.measurement_finished());
    }

    #[tokio::test]
    async fn enqueue_block_aligned_stops_when_duration_deadline_hits_first() {
        let sender = Address::repeat_byte(0x22);
        let results_tracker = ResultsTracker::new(&[sender]);
        results_tracker.begin_measurement(10, Some(100));
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::from([(sender, 0_u64)]);
        let mut rejected_senders = HashSet::new();
        let mut queued_gas = 0u128;
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel(8);
        let (_signed_chunk_tx, mut signed_chunk_rx) = mpsc::channel(1);
        let (base_fee_tx, _base_fee_rx) = tokio::sync::watch::channel(1_u128);
        let mut progress = EnqueueProgress { presigned_generated: 0, offered_gas: 0 };
        let mut presign_buffer = PresignBuffer::new(1);
        let pipeline = SubmissionPipeline::start(
            Arc::new(HashMap::new()),
            Arc::new(HashMap::new()),
            Arc::new(Vec::new()),
            results_tracker.clone(),
            mpsc::channel(1).0,
            PipelineStartConfig {
                chain_id: 1,
                max_gas_price: u128::MAX,
                validity_priority_lead_multiplier: 1,
                validity_priority_fee_divisor: 1,
                max_concurrent_submit_requests: None,
            },
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let stop_flag = AtomicBool::new(false);
        let (_pulse_tx, mut pulse_rx) = mpsc::channel(1);
        let (nonce_reset_tx, _nonce_reset_rx) = mpsc::channel(1);
        let sender_indices = HashMap::from([(sender, 0usize)]);

        tokio::time::timeout(
            Duration::from_secs(2),
            LoadRunner::enqueue_block_aligned_transactions(
                &mut PresignEnqueueState {
                    submission_pipeline: &pipeline,
                    next_submit_batch_id: &next_submit_batch_id,
                    signed_chunk_rx: &mut signed_chunk_rx,
                    nonce_reset_tx: &nonce_reset_tx,
                    sender_indices: &sender_indices,
                    buffer: &mut presign_buffer,
                    progress: &mut progress,
                    base_fee_tx: &base_fee_tx,
                },
                BlockAlignedEnqueueConfig {
                    controller: MempoolDepthController::new(
                        None,
                        Duration::from_secs(2),
                        1_000_000,
                        Instant::now(),
                    ),
                    fallback_block_gas_limit: 30_000_000,
                    block_time: Duration::from_secs(2),
                    presign_target_gas: 0,
                    max_in_flight_per_sender: 1,
                    max_total_in_flight: 1,
                    deadline: Some(Instant::now()),
                },
                &mut pulse_rx,
                &stop_flag,
                &mut EnqueueDrainState {
                    submit_event_rx: &mut submit_event_rx,
                    queued_per_sender: &mut queued_per_sender,
                    rejected_senders: &mut rejected_senders,
                    queued_gas: &mut queued_gas,
                    collector: &mut collector,
                    results_tracker: &results_tracker,
                    progress_display: None,
                },
            ),
        )
        .await
        .expect("enqueue loop should stop when deadline is reached")
        .expect("enqueue loop should exit cleanly");

        let summary = collector.summarize(Duration::from_secs(1), None);
        assert_eq!(summary.pacing.canonical_cycles, 0);
        assert!(!results_tracker.measurement_finished());
    }
}
