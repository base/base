//! Inventory headroom, presign streaming, and measured enqueue pacing.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::transaction::SignableTransaction;
use alloy_eips::Encodable2718;
use alloy_network::{Ethereum, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types::{BlockNumberOrTag, TransactionRequest};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_network::Base;
use base_tx_manager::NonceManager;
use tokio::{
    sync::{mpsc, watch},
    task,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use super::{
    BlockWatcher, DisplaySnapshot, FlashblockWatcher, LoadRunner, PipelineStartConfig,
    PreparedTransaction, QueuedSubmitFailures, ResultsTracker, SignedBatch, SignedTransaction,
    SubmissionPipeline, SubmitEvent, TxType,
};
use crate::{
    BaselineError, Result,
    metrics::{MetricsCollector, MetricsSummary},
    rpc::BaseFeeExt,
    workload::{KeyStream, SeededRng, WorkloadGenerator},
};

const NONCE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const SUBMIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMIT_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const PENDING_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(200);
const CONFIRMATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(200);
const SIGNED_BATCH_SIZE: usize = 200;
const PRESIGN_CHANNEL_BUFFER: usize = 2;
const IDLE_SLEEP: Duration = Duration::from_millis(10);
const HEADROOM_RECHECK_INTERVAL: Duration = Duration::from_millis(100);
const HEADROOM_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const PREFILL_TIMEOUT: Duration = Duration::from_secs(300);
/// Adaptive open-loop target update cadence, matching closed-loop rate-limiter updates.
const TARGET_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
/// Multiplicative headroom above observed confirmed TPS when deriving open-loop depth.
const TARGET_MARGIN_MULTIPLIER: f64 = 1.30;
/// Conversion window from observed TPS into outstanding transaction depth.
const TARGET_LOOKAHEAD_SECONDS: f64 = 1.5;
/// EWMA alpha for smoothing confirmed-TPS samples and reducing target oscillation.
const TARGET_TPS_EWMA_ALPHA: f64 = 0.35;
/// Minimum open-loop depth, expressed in signed-batch units.
const TARGET_MIN_BATCHES: u64 = 1;
/// Bootstrap open-loop depth before enough confirmations exist to adapt.
const TARGET_INITIAL_BATCHES: u64 = 2;
#[derive(Debug)]
struct SenderJob {
    sender_index: usize,
    from: Address,
    start_nonce: u64,
    prepared_txs: Vec<PreparedTransaction>,
}

#[derive(Debug)]
struct SignedSender {
    sender_index: usize,
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
    signers: Arc<HashMap<Address, PrivateKeySigner>>,
    chain_id: u64,
    base_fee_rx: watch::Receiver<u128>,
    max_gas_price: u128,
    fresh_recipient_ratio: f64,
    signed_chunk_tx: mpsc::Sender<Vec<Vec<SignedTransaction>>>,
}

struct EnqueueProgress {
    presigned_generated: u64,
    headroom_target: InFlightTarget,
}

#[derive(Debug, Clone, Copy)]
struct EnqueueLimits {
    deadline: Option<Instant>,
    stop_when_accepted_target_reached: bool,
}

struct EnqueueDrainState<'a> {
    submit_event_rx: &'a mut mpsc::Receiver<SubmitEvent>,
    queued_per_sender: &'a mut HashMap<Address, u64>,
    collector: &'a mut MetricsCollector,
    results_tracker: &'a ResultsTracker,
}

impl EnqueueDrainState<'_> {
    fn apply_submit_event(&mut self, event: SubmitEvent) {
        LoadRunner::apply_submit_event(event, self.queued_per_sender, self.collector);
    }

    fn drain_run_events(&mut self) {
        LoadRunner::drain_run_events(
            self.submit_event_rx,
            self.queued_per_sender,
            self.collector,
            self.results_tracker,
        );
    }

    /// Total outstanding work: submissions accepted by an RPC and awaiting canonical
    /// landing (`total_in_flight`) plus submissions handed to the submission pipeline
    /// but not yet accepted/rejected by an RPC (`queued_per_sender`).
    ///
    /// The headroom gate must count both, not just `total_in_flight`, otherwise a
    /// submission-pipeline backlog (e.g. a slow or saturated RPC) lets the enqueue loop
    /// race far past `target_in_flight` before the gate ever engages.
    fn total_outstanding(&self) -> u64 {
        let queued =
            self.queued_per_sender.values().fold(0u64, |total, count| total.saturating_add(*count));

        self.results_tracker.total_in_flight().saturating_add(queued)
    }
}

#[derive(Debug)]
struct InFlightTarget {
    current_target_in_flight: u64,
    min_target_in_flight: u64,
    max_target_in_flight: u64,
    target_outstanding_gas: Option<u128>,
    target_gps: Option<u64>,
    initial_avg_gas: u64,
    smoothed_confirmed_tps: f64,
    last_confirmed_sample_count: u64,
    last_confirmed_sample_at: Instant,
}

#[derive(Debug)]
struct InFlightTargetUpdate {
    previous_target_in_flight: u64,
    updated_target_in_flight: u64,
    confirmed_delta: u64,
    sample_tps: f64,
    smoothed_tps: f64,
}

impl InFlightTarget {
    fn new(
        max_target_in_flight: u64,
        target_gps: Option<u64>,
        initial_avg_gas: u64,
        initial_confirmed_count: u64,
        sampled_at: Instant,
    ) -> Self {
        if max_target_in_flight == 0 {
            return Self {
                current_target_in_flight: 0,
                min_target_in_flight: 0,
                max_target_in_flight: 0,
                target_outstanding_gas: None,
                target_gps,
                initial_avg_gas,
                smoothed_confirmed_tps: 0.0,
                last_confirmed_sample_count: initial_confirmed_count,
                last_confirmed_sample_at: sampled_at,
            };
        }

        let min_target_in_flight = (SIGNED_BATCH_SIZE as u64)
            .saturating_mul(TARGET_MIN_BATCHES)
            .min(max_target_in_flight);
        let initial_target_in_flight = (SIGNED_BATCH_SIZE as u64)
            .saturating_mul(TARGET_INITIAL_BATCHES)
            .clamp(min_target_in_flight, max_target_in_flight);

        let mut target = Self {
            current_target_in_flight: initial_target_in_flight,
            min_target_in_flight,
            max_target_in_flight,
            target_outstanding_gas: None,
            target_gps,
            initial_avg_gas,
            smoothed_confirmed_tps: 0.0,
            last_confirmed_sample_count: initial_confirmed_count,
            last_confirmed_sample_at: sampled_at,
        };
        target.current_target_in_flight =
            target.clamp_target_in_flight(initial_target_in_flight, None);
        target
    }

    const fn saturated(
        target_outstanding_gas: u128,
        target_in_flight: u64,
        max_target_in_flight: u64,
        initial_avg_gas: u64,
        sampled_at: Instant,
    ) -> Self {
        Self {
            current_target_in_flight: target_in_flight,
            min_target_in_flight: 1,
            max_target_in_flight,
            target_outstanding_gas: Some(target_outstanding_gas),
            target_gps: None,
            initial_avg_gas,
            smoothed_confirmed_tps: 0.0,
            last_confirmed_sample_count: 0,
            last_confirmed_sample_at: sampled_at,
        }
    }

    fn gas_derived_max_in_flight(&self, avg_gas_per_tx: Option<u64>) -> Option<u64> {
        let cap = self.target_gps?;
        let avg_gas = avg_gas_per_tx.unwrap_or(self.initial_avg_gas).max(1);
        Some(cap / avg_gas)
    }

    fn clamp_target_in_flight(&self, target_in_flight: u64, avg_gas_per_tx: Option<u64>) -> u64 {
        let clamped_to_bounds =
            target_in_flight.clamp(self.min_target_in_flight, self.max_target_in_flight);
        self.gas_derived_max_in_flight(avg_gas_per_tx)
            .map_or(clamped_to_bounds, |gas_derived_max| clamped_to_bounds.min(gas_derived_max))
    }

    const fn current_target_in_flight(&self) -> u64 {
        self.current_target_in_flight
    }

    fn maybe_update(
        &mut self,
        now: Instant,
        confirmed_count: u64,
        avg_gas_per_tx: Option<u64>,
    ) -> Option<InFlightTargetUpdate> {
        if now.saturating_duration_since(self.last_confirmed_sample_at)
            < TARGET_UPDATE_INTERVAL
        {
            return None;
        }

        let elapsed = now.saturating_duration_since(self.last_confirmed_sample_at).as_secs_f64();
        let confirmed_delta = confirmed_count.saturating_sub(self.last_confirmed_sample_count);
        let sample_tps = if elapsed > 0.0 { confirmed_delta as f64 / elapsed } else { 0.0 };

        if let Some(target_gas) = self.target_outstanding_gas {
            let average_gas = u128::from(avg_gas_per_tx.unwrap_or(self.initial_avg_gas).max(1));
            let updated_target_in_flight = u64::try_from(target_gas.div_ceil(average_gas))
                .unwrap_or(u64::MAX)
                .clamp(self.min_target_in_flight, self.max_target_in_flight);
            let previous_target_in_flight = self.current_target_in_flight;
            self.current_target_in_flight = updated_target_in_flight;
            self.last_confirmed_sample_count = confirmed_count;
            self.last_confirmed_sample_at = now;
            return Some(InFlightTargetUpdate {
                previous_target_in_flight,
                updated_target_in_flight,
                confirmed_delta,
                sample_tps,
                smoothed_tps: sample_tps,
            });
        }

        if self.smoothed_confirmed_tps == 0.0 {
            self.smoothed_confirmed_tps = sample_tps;
        } else {
            self.smoothed_confirmed_tps = self.smoothed_confirmed_tps
                * (1.0 - TARGET_TPS_EWMA_ALPHA)
                + sample_tps * TARGET_TPS_EWMA_ALPHA;
        }

        let adaptive_target = (self.smoothed_confirmed_tps
            * TARGET_MARGIN_MULTIPLIER
            * TARGET_LOOKAHEAD_SECONDS)
            .ceil() as u64;
        let target_with_batch_buffer =
            adaptive_target.saturating_add(SIGNED_BATCH_SIZE as u64);
        let updated_target_in_flight =
            self.clamp_target_in_flight(target_with_batch_buffer, avg_gas_per_tx);
        let previous_target_in_flight = self.current_target_in_flight;

        self.current_target_in_flight = updated_target_in_flight;
        self.last_confirmed_sample_count = confirmed_count;
        self.last_confirmed_sample_at = now;

        Some(InFlightTargetUpdate {
            previous_target_in_flight,
            updated_target_in_flight,
            confirmed_delta,
            sample_tps,
            smoothed_tps: self.smoothed_confirmed_tps,
        })
    }
}

impl LoadRunner {
    /// Computes the fixed transaction inventory required to hold the requested gas.
    pub fn mempool_target_transactions(
        block_gas_limit: u64,
        target_blocks: u64,
        average_gas: u64,
        capacity: u64,
    ) -> Result<u64> {
        if average_gas == 0 {
            return Err(BaselineError::Config("calibrated average gas must be > 0".into()));
        }
        let gas = u128::from(block_gas_limit)
            .checked_mul(u128::from(target_blocks))
            .ok_or_else(|| BaselineError::Config("mempool gas target overflowed".into()))?;
        let target = gas.div_ceil(u128::from(average_gas));
        let target = u64::try_from(target)
            .map_err(|_| BaselineError::Config("mempool transaction target exceeds u64".into()))?;
        if target > capacity {
            return Err(BaselineError::Config(format!(
                "mempool target requires {target} transactions but sender capacity is {capacity}"
            )));
        }
        Ok(target)
    }

    #[instrument(skip(self), fields(target_gps = ?self.config.target_gps, continuous = self.config.duration.is_none(), duration = ?self.config.duration))]
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
        let results_tracker = ResultsTracker::new(&sender_addresses);

        info!(url = %self.config.flashblocks_ws, "starting flashblock transaction watcher");
        let flashblock_watcher_task = Some(
            FlashblockWatcher::new(
                self.config.flashblocks_ws.clone(),
                results_tracker.clone(),
                self.cancel_token.clone(),
            )
            .start(),
        );

        info!(url = %self.config.query_rpc, "starting block watcher");
        let receipt_provider = RootProvider::<Base>::new_http(self.config.query_rpc.clone());
        let block_watcher_task = Some(
            BlockWatcher::new(
                receipt_provider.clone(),
                results_tracker.clone(),
                self.cancel_token.clone(),
            )
            .start(),
        );

        let max_in_flight_per_sender = self.config.max_in_flight_per_sender;

        let initial_avg_gas = self.calibrate_avg_gas().await?;
        for (address, nonce_manager) in self.nonce_managers.iter() {
            nonce_manager.reset().await;
            match nonce_manager.next_nonce().await {
                Ok(guard) => {
                    guard.rollback();
                    debug!(address = %address, "nonce manager pre-warmed");
                }
                Err(e) => {
                    warn!(address = %address, error = %e, "failed to pre-warm nonce manager");
                }
            }
        }
        // Seed the collector so live throughput (rolling GPS) and rate-limiter
        // feedback have a non-zero gas figure before canonical receipt gas lands.
        self.collector.set_estimated_gas(initial_avg_gas);
        let mut start = Instant::now();
        let account_count = self.accounts.len();

        info!(
            sender_count = account_count,
            max_sender_workers =
                SubmissionPipeline::sender_worker_count(self.submission_batch_rpcs.len()),
            max_in_flight_per_sender,
            initial_avg_gas,
            target_gps = self
                .config
                .target_gps
                .map_or_else(|| "unbounded".to_string(), |gps| format!("{gps} gas/s")),
            "starting load test in open-loop pre-signed mode"
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
            },
        );
        let next_submit_batch_id = AtomicU64::new(0);
        let mut queued_per_sender: HashMap<Address, u64> =
            self.accounts.accounts().iter().map(|a| (a.address, 0)).collect();

        let mut last_base_fee_refresh = Instant::now();
        let mut last_progress_report = Instant::now();
        let mut last_submitted_sample_count = 0u64;
        let mut last_submitted_sample_at = Instant::now();
        // Refresh once per block so the cached base fee tracks the climb the load
        // test itself induces; a stale fee mints underwater (unincludable) txs.
        const BASE_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
        const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
        const DISPLAY_RENDER_INTERVAL: Duration = Duration::from_millis(500);

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
        let sender_start_nonces = self.sender_start_nonces(&sender_addresses).await?;
        let sender_count = sender_addresses.len();

        let capacity = max_in_flight_per_sender.saturating_mul(account_count as u64);
        let open_loop_headroom_target = if self.config.target_gps.is_some() {
            InFlightTarget::new(
                capacity,
                self.config.target_gps,
                initial_avg_gas,
                self.collector.confirmed_count() as u64,
                Instant::now(),
            )
        } else {
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
            let target = Self::mempool_target_transactions(
                block_gas_limit,
                self.config.mempool_target_blocks,
                initial_avg_gas,
                capacity,
            )?;
            let target_gas =
                u128::from(block_gas_limit) * u128::from(self.config.mempool_target_blocks);
            InFlightTarget::saturated(
                target_gas,
                target,
                capacity,
                initial_avg_gas,
                Instant::now(),
            )
        };
        let initial_target_in_flight = open_loop_headroom_target.current_target_in_flight();

        let replacement_generator =
            WorkloadGenerator::from_tx_configs(self.workload_config(), &self.config.transactions, self.b20_run_salt)?;
        let producer_generator = std::mem::replace(&mut self.generator, replacement_generator);
        let producer_recipient_keys = self.recipient_keys.take();
        let producer_recipient_rng = std::mem::take(&mut self.recipient_rng);

        let (signed_chunk_tx, mut signed_chunk_rx) =
            mpsc::channel(PRESIGN_CHANNEL_BUFFER);
        let (base_fee_tx, base_fee_rx) = watch::channel(self.base_fee);
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
                signers: Arc::clone(&self.signers),
                chain_id: self.config.chain_id,
                base_fee_rx,
                max_gas_price: self.config.max_gas_price,
                fresh_recipient_ratio: self.config.fresh_recipient_ratio,
                signed_chunk_tx,
            },
        ));

        let mut progress = EnqueueProgress {
            presigned_generated: 0,
            headroom_target: open_loop_headroom_target,
        };

        info!(
            sender_count,
            initial_target_in_flight,
            max_target_in_flight = capacity,
            mempool_target_blocks = self.config.mempool_target_blocks,
            "started open-loop streaming pre-sign pipeline"
        );

        let prefill_deadline = Instant::now() + PREFILL_TIMEOUT;
        let mut prefill_result = Self::enqueue_signed_transactions(
            &submission_pipeline,
            &next_submit_batch_id,
            &mut signed_chunk_rx,
            &mut progress,
            EnqueueLimits {
                deadline: Some(prefill_deadline),
                stop_when_accepted_target_reached: true,
            },
            &self.stop_flag,
            &mut EnqueueDrainState {
                submit_event_rx: &mut submit_event_rx,
                queued_per_sender: &mut queued_per_sender,
                collector: &mut self.collector,
                results_tracker: &results_tracker,
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
                    &mut self.collector,
                    &results_tracker,
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
                &results_tracker,
            );
            let pending_batches = submission_pipeline.pending_batches();
            if pending_batches > 0 {
                prefill_result = Err(BaselineError::Timeout {
                    operation: format!(
                        "setup submission pipeline drain ({pending_batches} batches pending)"
                    ),
                    duration: SUBMIT_DRAIN_TIMEOUT,
                });
            }
        }

        if prefill_result.is_ok() {
            let ready_file = self.config.separate_setup.as_deref().map(|dir| dir.join("ready"));
            Self::publish_handshake(ready_file.as_deref())?;
            self.wait_for_start_file().await?;
            Self::drain_run_events(
                &mut submit_event_rx,
                &mut queued_per_sender,
                &mut self.collector,
                &results_tracker,
            );
            results_tracker.begin_measurement();
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
            Self::enqueue_signed_transactions(
                &submission_pipeline,
                &next_submit_batch_id,
                &mut signed_chunk_rx,
                &mut progress,
                EnqueueLimits {
                    deadline: enqueue_deadline,
                    stop_when_accepted_target_reached: false,
                },
                &self.stop_flag,
                &mut EnqueueDrainState {
                    submit_event_rx: &mut submit_event_rx,
                    queued_per_sender: &mut queued_per_sender,
                    collector: &mut self.collector,
                    results_tracker: &results_tracker,
                },
            )
            .await
        };

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
                presigned_generated = progress.presigned_generated,
                elapsed_secs = pre_sign_started.elapsed().as_secs_f64(),
                "open-loop pre-signed enqueue phase complete"
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

            if use_live_display || use_snapshot_tx {
                if last_progress_report.elapsed() >= DISPLAY_RENDER_INTERVAL {
                    self.collector.sample_throughput(start.elapsed());
                    let snap = self.build_snapshot(
                        start,
                        &results_tracker,
                        max_in_flight_per_sender,
                        account_count,
                    );
                    if let Some(ref d) = self.display {
                        d.update(&snap);
                    }
                    if let Some(ref tx) = self.snapshot_tx {
                        let _ = tx.send(snap);
                    }
                    last_progress_report = Instant::now();
                }
            } else if last_progress_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
                self.collector.sample_throughput(start.elapsed());
                let elapsed_secs = start.elapsed().as_secs();
                let submitted = self.collector.submitted_count();
                let confirmed = self.collector.confirmed_count();
                let failed = self.collector.failed_count();
                let in_flight = results_tracker.total_in_flight();
                let pending = results_tracker.pending_count();
                let senders_blocked = results_tracker.senders_at_limit(max_in_flight_per_sender);
                let total_queued: u64 = queued_per_sender.values().sum();
                let (p50, p99) = self.collector.rolling_p50_p99();
                let (flashblocks_p50, flashblocks_p99) =
                    self.collector.rolling_flashblocks_p50_p99();
                let report_now = Instant::now();
                let report_elapsed_secs =
                    report_now.saturating_duration_since(last_submitted_sample_at).as_secs_f64();
                let submitted_delta = submitted.saturating_sub(last_submitted_sample_count);
                let submitted_per_sec = if report_elapsed_secs > 0.0 {
                    submitted_delta as f64 / report_elapsed_secs
                } else {
                    0.0
                };
                info!(
                    elapsed_secs,
                    submitted,
                    submitted_per_sec,
                    confirmed,
                    failed,
                    in_flight,
                    pending,
                    total_queued,
                    senders_blocked,
                    presigned_generated = progress.presigned_generated,
                    base_fee = self.base_fee,
                    p50_ms = p50.as_millis() as u64,
                    p99_ms = p99.as_millis() as u64,
                    flashblocks_p50_ms = flashblocks_p50.as_millis() as u64,
                    flashblocks_p99_ms = flashblocks_p99.as_millis() as u64,
                    "progress"
                );
                last_submitted_sample_count = submitted;
                last_submitted_sample_at = report_now;
                last_progress_report = Instant::now();
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
                &mut self.collector,
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
            &mut self.collector,
        );

        // Keep background watchers alive through the drain so late flashblock
        // inclusions and block observations can still be joined into metrics.
        self.stop_flag.store(true, Ordering::SeqCst);

        if let Some(display) = &self.display {
            display.finish();
        }

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
        let results_poll_interval = Duration::from_millis(600);
        let mut last_confirmed_at = start.elapsed();

        while drain_start.elapsed() < CONFIRMATION_DRAIN_TIMEOUT {
            for (latency, observed_at) in results_tracker.drain_flashblock_observations() {
                self.collector.record_flashblock_observed(latency, observed_at);
            }
            let metrics = results_tracker.drain_confirmed_metrics();
            if !metrics.is_empty() {
                last_confirmed_at = start.elapsed();
                for metrics in metrics {
                    self.collector.record_confirmed(metrics);
                }
            }

            // Use a shorter expiry during drain: the test is over, so any
            // pending tx older than the drain window itself is stale.
            let drain_expiry = PENDING_CONFIRMATION_TIMEOUT.saturating_sub(drain_start.elapsed());
            let expired = results_tracker.expire_pending(drain_expiry);
            if expired > 0 {
                self.collector.record_failures("expired without confirmation", expired);
            }

            if results_tracker.pending_count() == 0 {
                break;
            }

            tokio::time::sleep(results_poll_interval).await;
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

        if let Some(task) = flashblock_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "flashblock watcher panicked"),
                _ => {}
            }
        }
        if let Some(task) = block_watcher_task {
            match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Err(e)) if e.is_panic() => warn!(error = %e, "block watcher panicked"),
                _ => {}
            }
        }

        let confirmed = self.collector.confirmed_count();
        info!(confirmed, submitted, "confirmation collection complete");

        // Fetch canonical receipts in a single batch pass, scoped to only the blocks
        // our transactions landed in, to backfill gas and revert status. This can be
        // slow on large runs, so notify the user before starting.
        let landed_blocks = results_tracker.landed_block_numbers();
        if !landed_blocks.is_empty() {
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

        let summary = self.collector.summarize_with_fresh_recipient_count(
            last_confirmed_at,
            self.config_summary.clone(),
            self.fresh_recipient_count(),
        );
        if let Some(fresh_recipient_count) = summary.fresh_recipient_count {
            info!(fresh_recipient_count, "fresh recipient generation complete");
        }

        if let Some(err) = open_loop_enqueue_error {
            return Err(err);
        }
        Ok(summary)
    }

    async fn sender_start_nonces(
        &self,
        sender_addresses: &[Address],
    ) -> Result<Vec<u64>> {
        let mut sender_start_nonces = Vec::with_capacity(sender_addresses.len());

        for from in sender_addresses {
            let Some(nonce_manager) = self.nonce_managers.get(from) else {
                return Err(BaselineError::Transaction(format!(
                    "missing nonce manager for sender {from}"
                )));
            };

            let nonce_guard = nonce_manager.next_nonce().await.map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to fetch starting nonce for sender {from}: {e}"
                ))
            })?;
            sender_start_nonces.push(nonce_guard.nonce());
            nonce_guard.rollback();
        }

        Ok(sender_start_nonces)
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

    fn build_sender_jobs(
        generator: &mut WorkloadGenerator,
        recipient_keys: &mut Option<KeyStream>,
        recipient_rng: &mut SeededRng,
        fresh_recipient_ratio: f64,
        sender_addresses: &[Address],
        sender_start_nonces: &[u64],
        txs_per_sender: usize,
    ) -> Result<Vec<SenderJob>> {
        if sender_addresses.len() != sender_start_nonces.len() {
            return Err(BaselineError::Transaction(format!(
                "open-loop sender nonce set mismatch: {} addresses vs {} nonces",
                sender_addresses.len(),
                sender_start_nonces.len(),
            )));
        }

        let sender_count = sender_addresses.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }
        let mut sender_jobs = Vec::with_capacity(sender_count);
        for (sender_index, from) in sender_addresses.iter().copied().enumerate() {
            let sender_pool_recipient = sender_addresses[(sender_index + 1) % sender_count];
            let mut prepared_txs = Vec::with_capacity(txs_per_sender);
            for _ in 0..txs_per_sender {
                let payload = generator.select_payload()?;
                let to = if payload.uses_runner_recipient() {
                    Self::select_recipient(
                        recipient_keys,
                        recipient_rng,
                        fresh_recipient_ratio,
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

                prepared_txs.push(PreparedTransaction {
                    from,
                    to: to_addr,
                    value,
                    data,
                    gas_limit,
                });
            }

            sender_jobs.push(SenderJob {
                sender_index,
                from,
                start_nonce: sender_start_nonces[sender_index],
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
    ) -> Result<Vec<Vec<SignedTransaction>>> {
        let sender_count = sender_jobs.len();
        if sender_count == 0 {
            return Ok(Vec::new());
        }

        let priority_fee = (base_fee / 10).max(1);
        let max_fee = SubmissionPipeline::submission_max_fee(base_fee, priority_fee, max_gas_price);

        let mut signing_tasks = Vec::with_capacity(sender_count);
        for sender_job in sender_jobs {
            let Some(signer) = signers.get(&sender_job.from).cloned() else {
                return Err(BaselineError::Transaction(format!(
                    "missing signer for sender {}",
                    sender_job.from
                )));
            };
            signing_tasks.push(task::spawn_blocking(move || {
                Self::sign_sender_job(sender_job, signer, chain_id, priority_fee, max_fee)
            }));
        }

        let mut signed_by_sender: Vec<Option<Vec<SignedTransaction>>> =
            std::iter::repeat_with(|| None).take(sender_count).collect();

        for signing_task in signing_tasks {
            let signed_sender = signing_task.await.map_err(|e| {
                BaselineError::Transaction(format!("open-loop signing task failed: {e}"))
            })??;

            let sender_index = signed_sender.sender_index;
            if signed_by_sender[sender_index].is_some() {
                return Err(BaselineError::Transaction(format!(
                    "duplicate signed sender result for index {sender_index}"
                )));
            }
            signed_by_sender[sender_index] = Some(signed_sender.signed_txs);
        }

        let mut ordered_signed_txs = Vec::with_capacity(sender_count);
        for (sender_index, sender_txs) in signed_by_sender.into_iter().enumerate() {
            let sender_txs = sender_txs.ok_or_else(|| {
                BaselineError::Transaction(format!(
                    "missing signed transaction set for sender index {sender_index}"
                ))
            })?;
            ordered_signed_txs.push(sender_txs);
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
        let mut chunk_index = 0usize;

        // The producer streams indefinitely and only stops when the consumer drops
        // `signed_chunk_rx` (detected below via the `send(...).is_err()` check), which
        // happens once the enqueue loop returns (deadline reached or channel closed).
        loop {
            let sender_jobs = Self::build_sender_jobs(
                &mut producer_state.generator,
                &mut producer_state.recipient_keys,
                &mut producer_state.recipient_rng,
                config.fresh_recipient_ratio,
                &config.sender_addresses,
                &config.sender_next_nonces,
                chunk_per_sender,
            )?;

            let base_fee = *config.base_fee_rx.borrow_and_update();
            let signed_by_sender = Self::sign_sender_jobs(
                sender_jobs,
                Arc::clone(&config.signers),
                config.chain_id,
                base_fee,
                config.max_gas_price,
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

            chunk_index = chunk_index.saturating_add(1);
            debug!(chunk_index, chunk_per_sender, "open-loop pre-sign producer generated chunk");
        }

        Ok(producer_state)
    }

    fn sign_sender_job(
        sender_job: SenderJob,
        signer: PrivateKeySigner,
        chain_id: u64,
        priority_fee: u128,
        max_fee: u128,
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

            let mut tx = TransactionRequest::default()
                .with_from(prepared.from)
                .with_value(prepared.value)
                .with_input(prepared.data)
                .with_nonce(nonce)
                .with_chain_id(chain_id)
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority_fee)
                .with_gas_limit(prepared.gas_limit);
            if let Some(to) = prepared.to {
                tx = tx.with_to(to);
            }

            let typed_tx = tx.build_typed_tx().map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to build typed tx for sender {} nonce {}: {e:?}",
                    prepared.from, nonce
                ))
            })?;

            let sig_hash = typed_tx.signature_hash();
            let signature = signer.sign_hash_sync(&sig_hash).map_err(|e| {
                BaselineError::Transaction(format!(
                    "failed to sign tx for sender {} nonce {}: {e}",
                    prepared.from, nonce
                ))
            })?;

            let signed = typed_tx.into_signed(signature);
            let tx_hash = *signed.hash();
            let raw = Bytes::from(signed.encoded_2718());

            signed_txs.push(SignedTransaction { raw, tx_hash, from: prepared.from, nonce });
        }

        Ok(SignedSender { sender_index: sender_job.sender_index, signed_txs })
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

    /// Blocks new submissions while outstanding work (submitted-but-unconfirmed
    /// `total_in_flight`, plus handed-to-the-pipeline-but-not-yet-RPC-accepted
    /// `queued_per_sender`) is at or above `target_in_flight`, draining confirmation
    /// and submit events in the meantime.
    ///
    /// This paces the open-loop fill to the builder's drain rate so the pool holds a
    /// steady depth instead of being flooded in one burst and then draining empty.
    /// A `target_in_flight` of 0 disables pacing (unbounded fill).
    ///
    /// Outstanding work is released by the block watcher mutating the shared
    /// `ResultsTracker` out-of-band, not by submit events, so the wait re-reads
    /// `total_outstanding` on a fixed interval rather than only when a submit event
    /// arrives; a `recv` timeout means "re-check", never "grant headroom". If
    /// outstanding work fails to fall for `HEADROOM_STALL_TIMEOUT` the
    /// confirmation path is dead and the gate fails closed (returns `Err`) instead of
    /// hanging or flooding the pool.
    async fn wait_for_outstanding_headroom(
        target_in_flight: u64,
        deadline: Option<Instant>,
        stop_when_accepted_target_reached: bool,
        stop_flag: &AtomicBool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Result<()> {
        if target_in_flight == 0 {
            return Ok(());
        }

        let mut last_outstanding = drain_state.total_outstanding();
        let mut last_progress = Instant::now();

        while drain_state.total_outstanding() >= target_in_flight {
            if stop_flag.load(Ordering::SeqCst) {
                return Err(BaselineError::Transaction("stopped during open-loop enqueue".into()));
            }
            if stop_when_accepted_target_reached
                && drain_state.results_tracker.total_in_flight() >= target_in_flight
            {
                return Ok(());
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                if stop_when_accepted_target_reached {
                    return Err(BaselineError::Timeout {
                        operation: "open-loop mempool prefill".into(),
                        duration: PREFILL_TIMEOUT,
                    });
                }
                return Ok(());
            }

            match tokio::time::timeout(
                HEADROOM_RECHECK_INTERVAL,
                drain_state.submit_event_rx.recv(),
            )
            .await
            {
                Ok(Some(event)) => {
                    drain_state.apply_submit_event(event);
                    drain_state.drain_run_events();
                }
                Ok(None) => return Ok(()),
                Err(_) => drain_state.drain_run_events(),
            }

            let current = drain_state.total_outstanding();
            if current < last_outstanding {
                last_outstanding = current;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= HEADROOM_STALL_TIMEOUT {
                return Err(BaselineError::Timeout {
                    operation: format!(
                        "open-loop outstanding headroom (stuck at {current} outstanding, target {target_in_flight})"
                    ),
                    duration: HEADROOM_STALL_TIMEOUT,
                });
            }
        }

        Ok(())
    }

    async fn enqueue_signed_transactions(
        submission_pipeline: &SubmissionPipeline,
        next_submit_batch_id: &AtomicU64,
        signed_chunk_rx: &mut mpsc::Receiver<Vec<Vec<SignedTransaction>>>,
        progress: &mut EnqueueProgress,
        limits: EnqueueLimits,
        stop_flag: &AtomicBool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> Result<()> {
        let mut pending_signed_batch = Vec::with_capacity(SIGNED_BATCH_SIZE);
        let mut setup_target_reached = false;

        loop {
            drain_state.drain_run_events();
            if stop_flag.load(Ordering::SeqCst) {
                return Err(BaselineError::Transaction("stopped during open-loop enqueue".into()));
            }
            if limits.stop_when_accepted_target_reached
                && drain_state.results_tracker.total_in_flight()
                    >= progress.headroom_target.current_target_in_flight()
            {
                if !pending_signed_batch.is_empty()
                    && !Self::enqueue_signed_batch(
                        submission_pipeline,
                        next_submit_batch_id,
                        &mut pending_signed_batch,
                        false,
                        drain_state,
                    )
                    .await
                {
                    return Err(BaselineError::Transaction(
                        "submit queue closed while flushing setup nonce range".into(),
                    ));
                }
                return Ok(());
            }
            if limits.deadline.is_some_and(|d| Instant::now() >= d) {
                if limits.stop_when_accepted_target_reached {
                    return Err(BaselineError::Timeout {
                        operation: "open-loop mempool prefill".into(),
                        duration: PREFILL_TIMEOUT,
                    });
                }
                return Ok(());
            }

            let signed_by_sender = match tokio::time::timeout(
                HEADROOM_RECHECK_INTERVAL,
                signed_chunk_rx.recv(),
            )
            .await
            {
                Ok(Some(signed_by_sender)) => signed_by_sender,
                Ok(None) => break,
                Err(_) => continue,
            };

            for sender_signed in &signed_by_sender {
                progress.presigned_generated =
                    progress.presigned_generated.saturating_add(sender_signed.len() as u64);
            }

            let mut sender_iters =
                signed_by_sender.into_iter().map(Vec::into_iter).collect::<Vec<_>>();

            loop {
                let mut made_progress = false;

                for sender_iter in &mut sender_iters {
                    if let Some(signed_tx) = sender_iter.next() {
                        made_progress = true;
                        pending_signed_batch.push(signed_tx);

                        if pending_signed_batch.len() >= SIGNED_BATCH_SIZE {
                            if !setup_target_reached {
                                if let Some(update) = progress.headroom_target.maybe_update(
                                    Instant::now(),
                                    drain_state.collector.confirmed_count() as u64,
                                    drain_state.results_tracker.observed_avg_gas(),
                                ) {
                                    debug!(
                                        previous_target_in_flight =
                                            update.previous_target_in_flight,
                                        updated_target_in_flight = update.updated_target_in_flight,
                                        confirmed_delta = update.confirmed_delta,
                                        sample_tps = update.sample_tps,
                                        smoothed_tps = update.smoothed_tps,
                                        "adjusted open-loop in-flight target"
                                    );
                                }
                                Self::wait_for_outstanding_headroom(
                                    progress.headroom_target.current_target_in_flight(),
                                    limits.deadline,
                                    limits.stop_when_accepted_target_reached,
                                    stop_flag,
                                    drain_state,
                                )
                                .await?;
                                setup_target_reached = limits.stop_when_accepted_target_reached
                                    && drain_state.results_tracker.total_in_flight()
                                        >= progress.headroom_target.current_target_in_flight();
                                if !setup_target_reached
                                    && limits.deadline.is_some_and(|d| Instant::now() >= d)
                                {
                                    if limits.stop_when_accepted_target_reached {
                                        return Err(BaselineError::Timeout {
                                            operation: "open-loop mempool prefill".into(),
                                            duration: PREFILL_TIMEOUT,
                                        });
                                    }
                                    return Ok(());
                                }
                            }
                            if !Self::enqueue_signed_batch(
                                submission_pipeline,
                                next_submit_batch_id,
                                &mut pending_signed_batch,
                                !limits.stop_when_accepted_target_reached,
                                drain_state,
                            )
                            .await
                            {
                                if limits.stop_when_accepted_target_reached {
                                    return Err(BaselineError::Transaction(
                                        "submit queue closed during setup prefill".into(),
                                    ));
                                }
                                return Ok(());
                            }
                        }
                    }
                }

                if !made_progress {
                    break;
                }
            }

            if setup_target_reached {
                if !pending_signed_batch.is_empty()
                    && !Self::enqueue_signed_batch(
                        submission_pipeline,
                        next_submit_batch_id,
                        &mut pending_signed_batch,
                        false,
                        drain_state,
                    )
                    .await
                {
                    return Err(BaselineError::Transaction(
                        "submit queue closed while flushing setup nonce range".into(),
                    ));
                }
                return Ok(());
            }

            drain_state.drain_run_events();
        }

        if !pending_signed_batch.is_empty() {
            if let Some(update) = progress.headroom_target.maybe_update(
                Instant::now(),
                drain_state.collector.confirmed_count() as u64,
                drain_state.results_tracker.observed_avg_gas(),
            ) {
                debug!(
                    previous_target_in_flight = update.previous_target_in_flight,
                    updated_target_in_flight = update.updated_target_in_flight,
                    confirmed_delta = update.confirmed_delta,
                    sample_tps = update.sample_tps,
                    smoothed_tps = update.smoothed_tps,
                    "adjusted open-loop in-flight target"
                );
            }
            Self::wait_for_outstanding_headroom(
                progress.headroom_target.current_target_in_flight(),
                limits.deadline,
                limits.stop_when_accepted_target_reached,
                stop_flag,
                drain_state,
            )
            .await?;
            let enqueued = Self::enqueue_signed_batch(
                submission_pipeline,
                next_submit_batch_id,
                &mut pending_signed_batch,
                !limits.stop_when_accepted_target_reached,
                drain_state,
            )
            .await;
            if !enqueued && limits.stop_when_accepted_target_reached {
                return Err(BaselineError::Transaction(
                    "submit queue closed while flushing setup nonce range".into(),
                ));
            }
        }

        Ok(())
    }

    async fn enqueue_signed_batch(
        submission_pipeline: &SubmissionPipeline,
        next_submit_batch_id: &AtomicU64,
        pending_signed_batch: &mut Vec<SignedTransaction>,
        measured: bool,
        drain_state: &mut EnqueueDrainState<'_>,
    ) -> bool {
        let signed_txs = std::mem::replace(
            pending_signed_batch,
            Vec::with_capacity(SIGNED_BATCH_SIZE),
        );
        let batch_len = signed_txs.len();
        let batch_id = next_submit_batch_id.fetch_add(1, Ordering::SeqCst);

        for signed_tx in &signed_txs {
            drain_state
                .queued_per_sender
                .entry(signed_tx.from)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }

        let batch = SignedBatch { id: batch_id, attempt: 0, measured, txs: signed_txs };
        match Self::enqueue_signed_while_draining(submission_pipeline, batch, drain_state).await {
            Ok(()) => {
                debug!(batch_id, batch_len, "queued open-loop signed batch");
                true
            }
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
        max_in_flight_per_sender: u64,
        account_count: usize,
    ) -> DisplaySnapshot {
        let (p50, p99) = self.collector.rolling_p50_p99();
        let (flashblocks_p50, flashblocks_p99) = self.collector.rolling_flashblocks_p50_p99();
        DisplaySnapshot {
            elapsed: start.elapsed(),
            duration: self.config.duration,
            submitted: self.collector.submitted_count(),
            confirmed: self.collector.confirmed_count(),
            failed: self.collector.failed_count(),
            in_flight: results_tracker.total_in_flight(),
            senders_blocked: results_tracker.senders_at_limit(max_in_flight_per_sender),
            total_senders: account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            p50_latency: p50,
            p99_latency: p99,
            flashblocks_p50_latency: flashblocks_p50,
            flashblocks_p99_latency: flashblocks_p99,
            gas_price_gwei: self.base_fee as f64 / 1e9,
            total_eth: self.last_total_eth.clone(),
            min_eth: self.last_min_eth.clone(),
            funds_low: self.last_funds_low,
            funder_address: self.funder_address.clone(),
            sender_addresses: self.sender_addresses.clone(),
        }
    }

    fn apply_submit_event(
        event: SubmitEvent,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        match event {
            SubmitEvent::Submitted(tx_hash) => collector.record_submitted(tx_hash),
            SubmitEvent::Failed(reason) => {
                collector.record_failed(TxHash::ZERO, &reason);
            }
            SubmitEvent::Released(from) => {
                if let Some(count) = queued_per_sender.get_mut(&from) {
                    *count = count.saturating_sub(1);
                }
            }
        }
    }

    fn drain_submit_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(event, queued_per_sender, collector);
        }
    }

    fn drain_run_events(
        submit_event_rx: &mut mpsc::Receiver<SubmitEvent>,
        queued_per_sender: &mut HashMap<Address, u64>,
        collector: &mut MetricsCollector,
        results_tracker: &ResultsTracker,
    ) {
        while let Ok(event) = submit_event_rx.try_recv() {
            Self::apply_submit_event(event, queued_per_sender, collector);
        }
        for metrics in results_tracker.drain_confirmed_metrics() {
            collector.record_confirmed(metrics);
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
        collections::HashMap,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    use alloy_primitives::{Address, Bytes, TxHash};
    use tokio::sync::mpsc;

    use super::{
        EnqueueDrainState, InFlightTarget, LoadRunner, SIGNED_BATCH_SIZE,
        TARGET_LOOKAHEAD_SECONDS, TARGET_MARGIN_MULTIPLIER, TARGET_UPDATE_INTERVAL,
    };
    use crate::{
        metrics::MetricsCollector,
        runner::{
            PipelineStartConfig, ResultsTracker, SignedBatch, SignedTransaction, SubmissionPipeline,
            SubmitEvent, SUBMIT_BATCH_QUEUE_BUFFER,
        },
    };
    #[test]
    fn mempool_target_uses_ceiling_and_checks_capacity() {
        assert_eq!(LoadRunner::mempool_target_transactions(100, 3, 70, 10).unwrap(), 5);
        assert!(LoadRunner::mempool_target_transactions(100, 3, 70, 4).is_err());
        assert!(LoadRunner::mempool_target_transactions(100, 3, 0, 10).is_err());
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

        let (submit_event_tx, mut submit_event_rx) = mpsc::channel(1);
        let mut submission_pipeline = SubmissionPipeline::start(
            Arc::new(HashMap::new()),
            Arc::new(HashMap::new()),
            Arc::new(Vec::new()),
            results_tracker.clone(),
            submit_event_tx.clone(),
            PipelineStartConfig { chain_id: 1, max_gas_price: u128::MAX },
        );

        submission_pipeline.shutdown_and_join(Duration::from_secs(1)).await;

        for batch_id in 0..SUBMIT_BATCH_QUEUE_BUFFER {
            submission_pipeline
                .enqueue_signed(test_signed_batch(batch_id as u64, sender))
                .await
                .expect("signed queue should accept up to capacity");
        }

        submit_event_tx
            .send(SubmitEvent::Released(sender))
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
                collector: &mut collector,
                results_tracker: &results_tracker,
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

    #[tokio::test]
    async fn headroom_gate_resumes_when_block_watcher_lowers_in_flight_without_submit_event() {
        use std::time::Instant;

        use super::super::results_tracker::{BlockObservation, SentTransaction};

        let sender = Address::repeat_byte(0x22);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        let tx_hash = TxHash::repeat_byte(0x22);
        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash,
            from: sender,
            measured: true,
        }]);
        assert_eq!(results_tracker.total_in_flight(), 1);

        let tracker_for_watcher = results_tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            tracker_for_watcher.on_new_block_hashes(
                BlockObservation { number: 1, observed_at: Instant::now() },
                vec![tx_hash],
            );
        });

        let mut drain_state = EnqueueDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            LoadRunner::wait_for_outstanding_headroom(
                1,
                None,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("gate must not hang when in-flight is released out-of-band");

        assert!(result.is_ok(), "gate should resume once in-flight drops below target");
        assert_eq!(results_tracker.total_in_flight(), 0);
    }

    #[tokio::test]
    async fn headroom_gate_returns_immediately_when_pacing_disabled() {
        use super::super::results_tracker::SentTransaction;

        let sender = Address::repeat_byte(0x44);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash: TxHash::repeat_byte(0x44),
            from: sender,
            measured: true,
        }]);

        let mut drain_state = EnqueueDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            LoadRunner::wait_for_outstanding_headroom(
                0,
                None,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("target 0 must return without waiting even while in-flight is high");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn headroom_gate_returns_ok_when_past_deadline() {
        use std::time::Instant;

        use super::super::results_tracker::SentTransaction;

        let sender = Address::repeat_byte(0x55);
        let results_tracker = ResultsTracker::new(&[sender]);
        let mut collector = MetricsCollector::new();
        let mut queued_per_sender = HashMap::new();
        let (_submit_event_tx, mut submit_event_rx) = mpsc::channel::<SubmitEvent>(1);

        results_tracker.sent_transactions(vec![SentTransaction {
            tx_hash: TxHash::repeat_byte(0x55),
            from: sender,
            measured: true,
        }]);

        let mut drain_state = EnqueueDrainState {
            submit_event_rx: &mut submit_event_rx,
            queued_per_sender: &mut queued_per_sender,
            collector: &mut collector,
            results_tracker: &results_tracker,
        };

        let past_deadline = Some(Instant::now() - Duration::from_secs(1));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            LoadRunner::wait_for_outstanding_headroom(
                1,
                past_deadline,
                false,
                &AtomicBool::new(false),
                &mut drain_state,
            ),
        )
        .await
        .expect("gate must stop waiting once the load window deadline has passed");
        assert!(result.is_ok(), "past-deadline gate returns Ok without failing closed");
        assert_eq!(
            results_tracker.total_in_flight(),
            1,
            "in-flight stays high; gate returns because window ended, not because it drained"
        );
    }

    #[test]
    fn open_loop_headroom_target_clamps_to_target_gps_cap() {
        let now = Instant::now();
        let sampled_at = now - TARGET_UPDATE_INTERVAL;
        let mut target =
            InFlightTarget::new(10_000, Some(2_100_000), 21_000, 0, sampled_at);

        let update = target
            .maybe_update(now, 1_000, None)
            .expect("elapsed update interval should produce a new target");

        // Old behavior (without a gas-rate cap) would produce this larger target:
        // sample_tps=500 over 2s, then EWMA/margin/lookahead plus one batch buffer.
        let unclamped_target =
            ((500.0 * TARGET_MARGIN_MULTIPLIER * TARGET_LOOKAHEAD_SECONDS)
                .ceil() as u64)
                .saturating_add(SIGNED_BATCH_SIZE as u64);
        assert!(
            unclamped_target > 100,
            "test setup must produce a cap effect; unclamped target={unclamped_target}"
        );
        assert_eq!(update.updated_target_in_flight, 100);
        assert_eq!(target.current_target_in_flight(), 100);
    }

    #[test]
    fn saturated_headroom_target_tracks_observed_average_gas() {
        let now = Instant::now();
        let mut target = InFlightTarget::saturated(
            300_000,
            3,
            10,
            100_000,
            now - TARGET_UPDATE_INTERVAL,
        );

        let update = target
            .maybe_update(now, 1, Some(60_000))
            .expect("elapsed update interval should recalibrate the inventory");

        assert_eq!(update.updated_target_in_flight, 5);
        assert_eq!(target.current_target_in_flight(), 5);
    }

    #[test]
    fn open_loop_headroom_target_without_cap_matches_previous_formula() {
        let now = Instant::now();
        let sampled_at = now - TARGET_UPDATE_INTERVAL;
        let mut target = InFlightTarget::new(10_000, None, 21_000, 0, sampled_at);

        let update = target
            .maybe_update(now, 1_000, Some(21_000))
            .expect("elapsed update interval should produce a new target");

        let expected =
            ((500.0 * TARGET_MARGIN_MULTIPLIER * TARGET_LOOKAHEAD_SECONDS)
                .ceil() as u64)
                .saturating_add(SIGNED_BATCH_SIZE as u64)
                .clamp(SIGNED_BATCH_SIZE as u64, 10_000);
        assert_eq!(update.updated_target_in_flight, expected);
        assert_eq!(target.current_target_in_flight(), expected);
    }
}
