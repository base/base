//! Run coordinator: the single owner of submission, inclusion, and pacing bookkeeping.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    ops::Range,
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::{
    BlockMatch, BlockPulse, BlockWatcher, CanonicalBlock, DisplaySnapshot, LoadTestDisplay,
    LoadTestStage, Measurement, RefillLedger, ResultsTracker, RunEvent, SignedBatch,
    SignedTransaction, SubmissionPipeline, TxOutcome, TxResult,
};
use crate::metrics::MetricsCollector;

/// Live display refresh cadence.
pub const REPORT_INTERVAL: Duration = Duration::from_millis(500);
/// Structured progress log cadence.
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// Minimum spacing of "block matched nothing" warnings.
const UNMATCHED_WARNING_INTERVAL: Duration = Duration::from_secs(15);

/// Live progress display and structured progress logs for the current stage.
pub struct ProgressReport<'a> {
    /// Live footer, when attached.
    pub display: Option<&'a LoadTestDisplay>,
    /// Lifecycle stage shown and logged.
    pub stage: LoadTestStage,
    /// Start of the measured run, or of prefill before it.
    pub start: Instant,
    /// Configured measured duration.
    pub duration: Option<Duration>,
    /// Per-sender in-flight limit, for the blocked-sender count.
    pub max_in_flight_per_sender: u64,
    /// Number of senders.
    pub account_count: usize,
    /// Configured gas-per-second target.
    pub target_gps: Option<u64>,
    /// Latest canonical base fee.
    pub base_fee: watch::Receiver<u128>,
    /// When progress was last logged.
    pub last_log: Instant,
}

impl fmt::Debug for ProgressReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressReport")
            .field("stage", &self.stage)
            .field("duration", &self.duration)
            .finish_non_exhaustive()
    }
}

/// Owns all run bookkeeping and applies every task's [`RunEvent`] in arrival order.
///
/// Sender workers, chain watchers, and the receipt sampler hold only the inbox sender, so no
/// bookkeeping is shared or locked. Every wait in the run goes through the coordinator so the
/// inbox keeps draining: a full inbox would otherwise stall the workers that the wait is for.
#[derive(Debug)]
pub struct RunCoordinator<'a> {
    /// Tracked pending and confirmed transactions.
    pub tracker: ResultsTracker,
    /// Outstanding submit batches of each refill cycle.
    pub ledger: RefillLedger,
    /// Run-wide metrics collector.
    pub collector: &'a mut MetricsCollector,
    /// Live progress display and structured log state.
    pub report: ProgressReport<'a>,
    inbox: mpsc::Receiver<RunEvent>,
    inbox_open: bool,
    queued: Vec<u64>,
    queued_count: u64,
    queued_gas: u128,
    rejected_senders: Vec<usize>,
    outstanding_batches: u64,
    next_batch_id: u64,
    pulses: VecDeque<BlockPulse>,
    receipt_sampler: mpsc::Sender<u64>,
    next_report: Instant,
    last_unmatched_warning: Instant,
}

impl<'a> RunCoordinator<'a> {
    /// Creates a coordinator for the tracker's senders.
    pub fn new(
        inbox: mpsc::Receiver<RunEvent>,
        tracker: ResultsTracker,
        collector: &'a mut MetricsCollector,
        receipt_sampler: mpsc::Sender<u64>,
        report: ProgressReport<'a>,
    ) -> Self {
        if let Some(display) = report.display {
            display.set_stage(report.stage);
        }
        let now = Instant::now();
        Self {
            queued: vec![0; tracker.in_flight_by_sender().len()],
            tracker,
            ledger: RefillLedger::default(),
            collector,
            report,
            inbox,
            inbox_open: true,
            queued_count: 0,
            queued_gas: 0,
            rejected_senders: Vec::new(),
            outstanding_batches: 0,
            next_batch_id: 0,
            pulses: VecDeque::new(),
            receipt_sampler,
            next_report: now + REPORT_INTERVAL,
            // The first blocks after submission starts often precede any landing.
            last_unmatched_warning: now,
        }
    }

    /// Updates the lifecycle stage on the report and any attached live display.
    pub fn set_stage(&mut self, stage: LoadTestStage) {
        self.report.stage = stage;
        if let Some(display) = self.report.display {
            display.set_stage(stage);
        }
    }

    /// Returns local submission backlog gas not yet reported by a sender worker.
    pub const fn queued_gas(&self) -> u128 {
        self.queued_gas
    }

    /// Returns queued plus in-progress submit batches.
    pub const fn outstanding_batches(&self) -> u64 {
        self.outstanding_batches
    }

    /// Returns the id the next enqueued batch will get.
    pub const fn next_batch_id(&self) -> u64 {
        self.next_batch_id
    }

    /// Returns the free submission slots of each sender into `slots`.
    pub fn sender_slots(&self, max_per_sender: u64, slots: &mut Vec<u64>) {
        slots.clear();
        slots.extend(
            self.queued
                .iter()
                .zip(self.tracker.in_flight_by_sender())
                .map(|(queued, in_flight)| max_per_sender.saturating_sub(queued + in_flight)),
        );
    }

    /// Returns free run-wide submission slots under `capacity`.
    pub fn remaining_transaction_slots(&self, capacity: usize) -> usize {
        let occupied = self.tracker.total_in_flight().saturating_add(self.queued_count);
        capacity.saturating_sub(usize::try_from(occupied).unwrap_or(usize::MAX))
    }

    /// Removes and returns senders that had a submission fail since the last call, ascending and
    /// without duplicates.
    pub fn take_rejected_senders(&mut self) -> Vec<usize> {
        let mut senders = std::mem::take(&mut self.rejected_senders);
        senders.sort_unstable();
        senders.dedup();
        senders
    }

    /// Returns a sender to the resync set, for when its reset could not be delivered yet.
    pub fn requeue_rejected_sender(&mut self, sender: usize) {
        self.rejected_senders.push(sender);
    }

    /// Takes the oldest canonical block not yet paced.
    pub fn pop_pulse(&mut self) -> Option<BlockPulse> {
        self.pulses.pop_front()
    }

    /// Applies every event already waiting in the inbox.
    pub fn drain_inbox(&mut self) {
        while let Ok(event) = self.inbox.try_recv() {
            self.apply(event);
        }
        self.report_if_due();
    }

    /// Waits for and applies the next event while keeping the progress report on schedule.
    /// Returns false once every sender is gone, after which this never resolves, so it can stay
    /// in a `select!` without spinning.
    pub async fn next_event(&mut self) -> bool {
        loop {
            self.report_if_due();
            if !self.inbox_open {
                return std::future::pending().await;
            }
            let next_report = self.next_report.into();
            tokio::select! {
                biased;
                event = self.inbox.recv() => match event {
                    Some(event) => {
                        self.apply(event);
                        self.report_if_due();
                        return true;
                    }
                    None => {
                        self.inbox_open = false;
                        return false;
                    }
                },
                () = tokio::time::sleep_until(next_report) => {}
            }
        }
    }

    /// Drives `fut` to completion while applying events and refreshing progress.
    pub async fn pump_until<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                output = &mut fut => return output,
                _ = self.next_event() => {}
            }
        }
    }

    /// Applies events until `busy` turns false or `deadline` passes. Returns whether `busy`
    /// cleared.
    pub async fn pump_while(
        &mut self,
        deadline: Instant,
        mut busy: impl FnMut(&Self) -> bool,
    ) -> bool {
        let deadline = tokio::time::sleep_until(deadline.into());
        tokio::pin!(deadline);
        while busy(self) {
            tokio::select! {
                biased;
                () = &mut deadline => return false,
                alive = self.next_event() => if !alive { return !busy(self) },
            }
        }
        true
    }

    /// Applies events until nothing is pending, `timeout` passes, or neither the pending count
    /// shrinks nor a confirmation lands for `stall`. Returns when the last confirmation landed.
    ///
    /// A stall means the remainder is mempool inventory at the measurement cutoff: it stops being
    /// tracked and is recorded as undrained rather than failed.
    pub async fn drain_confirmations(
        &mut self,
        timeout: Duration,
        stall: Duration,
    ) -> Option<Instant> {
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let stalled = tokio::time::sleep(stall);
        tokio::pin!(stalled);
        let mut last_confirmed = None;
        let mut pending = self.tracker.pending_count();
        let mut confirmed = self.collector.confirmed_count();
        while pending > 0 {
            tokio::select! {
                biased;
                () = &mut deadline => break,
                () = &mut stalled => {
                    let (stalled, undrained_gas) = self.tracker.measured_unconfirmed_inventory();
                    info!(
                        stalled,
                        undrained_gas,
                        stall_secs = stall.as_secs(),
                        "confirmation drain stopped with expected undrained inventory"
                    );
                    break;
                }
                alive = self.next_event() => {
                    if !alive {
                        break;
                    }
                    let now_pending = self.tracker.pending_count();
                    let now_confirmed = self.collector.confirmed_count();
                    if now_confirmed > confirmed {
                        last_confirmed = Some(Instant::now());
                    }
                    if now_pending < pending || now_confirmed > confirmed {
                        stalled.as_mut().reset(tokio::time::Instant::now() + stall);
                    }
                    pending = now_pending;
                    confirmed = now_confirmed;
                }
            }
        }
        if self.tracker.pending_count() > 0 {
            let (undrained, undrained_gas) = self.tracker.drain_pending();
            self.collector.record_undrained_inventory(undrained, undrained_gas);
        }
        last_confirmed
    }

    /// Enqueues `txs` as one batch while draining the inbox. Returns false when the submit
    /// queue is closed, after recording the batch as failed.
    pub async fn enqueue(
        &mut self,
        pipeline: &SubmissionPipeline,
        txs: Vec<SignedTransaction>,
        measured: bool,
    ) -> bool {
        self.queued_count = self.queued_count.saturating_add(txs.len() as u64);
        for tx in &txs {
            if let Some(sender) = self.tracker.sender_index(&tx.from) {
                self.queued[sender] += 1;
            }
            self.queued_gas = self.queued_gas.saturating_add(u128::from(tx.estimated_gas));
        }
        let id = self.next_batch_id;
        self.next_batch_id += 1;
        self.outstanding_batches += 1;
        let batch = SignedBatch { id, attempt: 0, measured, txs };
        let Err(batch) = self.pump_until(pipeline.enqueue(batch)).await else {
            return true;
        };
        warn!(batch_id = id, batch_len = batch.len(), "submit queue closed while enqueuing batch");
        self.abandon(vec![batch], "submit queue closed");
        false
    }

    /// Releases batches no worker will send and records their transactions as failed.
    pub fn abandon(&mut self, batches: Vec<SignedBatch>, reason: &'static str) {
        let warmup = self.tracker.measurement() == Measurement::Warmup;
        let mut failed = 0u64;
        for batch in batches {
            self.outstanding_batches = self.outstanding_batches.saturating_sub(1);
            self.ledger.abandon(batch.id);
            for tx in &batch.txs {
                self.release_queued(tx.from, tx.estimated_gas);
            }
            if batch.measured || warmup {
                failed += batch.len() as u64;
            }
        }
        if failed > 0 {
            self.collector.record_failures(reason, failed);
        }
    }

    /// Waits until a refill cycle's batches are acknowledged or `deadline` passes. Returns the
    /// lag when acknowledged; a late cycle's lag is recorded when its last batch completes.
    pub async fn wait_for_refill(
        &mut self,
        batches: Range<u64>,
        deadline: Instant,
    ) -> Option<Duration> {
        self.pump_while(deadline, |coordinator| !coordinator.ledger.is_acked(&batches)).await;
        self.ledger.stop_waiting(&batches)
    }

    /// Refreshes the live display and logs progress when due.
    pub fn report(&mut self) {
        self.next_report = Instant::now() + REPORT_INTERVAL;
        let should_render = self.report.display.is_some();
        let should_log = self.report.last_log.elapsed() >= PROGRESS_LOG_INTERVAL;
        if !should_render && !should_log {
            return;
        }
        let elapsed = self.report.start.elapsed();
        if matches!(self.report.stage, LoadTestStage::Prefill | LoadTestStage::Submitting) {
            self.collector.sample_throughput(elapsed);
        }
        if should_log {
            info!(
                stage = self.report.stage.as_str(),
                elapsed_secs = elapsed.as_secs(),
                remaining_secs =
                    self.report.duration.map(|duration| duration.saturating_sub(elapsed).as_secs()),
                submitted = self.collector.submitted_count(),
                confirmed = self.collector.confirmed_count(),
                failed = self.collector.failed_count(),
                in_flight = self.tracker.total_in_flight(),
                rolling_gps = self.collector.rolling_gps(),
                "load test progress"
            );
            self.report.last_log = Instant::now();
        }
        if let Some(display) = self.report.display {
            display.update(&self.snapshot(elapsed));
        }
    }

    /// Builds the live display snapshot.
    pub fn snapshot(&mut self, elapsed: Duration) -> DisplaySnapshot {
        let (p50, p99) = self.collector.rolling_p50_p99();
        DisplaySnapshot {
            elapsed,
            duration: self.report.duration,
            stage: self.report.stage,
            submitted: self.collector.submitted_count(),
            confirmed: self.collector.confirmed_count(),
            failed: self.collector.failed_count(),
            in_flight: self.tracker.total_in_flight(),
            senders_blocked: self.tracker.senders_at_limit(self.report.max_in_flight_per_sender),
            total_senders: self.report.account_count,
            rolling_tps: self.collector.rolling_tps(),
            rolling_gps: self.collector.rolling_gps(),
            target_gps: self.report.target_gps,
            p50_latency: p50,
            p99_latency: p99,
            gas_price_gwei: *self.report.base_fee.borrow() as f64 / 1e9,
        }
    }

    /// Moves confirmed transaction metrics into the collector.
    pub fn record_confirmations(&mut self) -> bool {
        let mut any = false;
        for metrics in self.tracker.drain_confirmed_metrics() {
            self.collector.record_confirmed(metrics);
            any = true;
        }
        any
    }

    fn report_if_due(&mut self) {
        if Instant::now() >= self.next_report {
            self.report();
        }
    }

    fn apply(&mut self, event: RunEvent) {
        match event {
            RunEvent::Sending { txs, at, at_wall } => {
                self.tracker.sent_transactions(txs, at, at_wall);
            }
            RunEvent::Settled(results) => {
                for result in results {
                    self.settle(result);
                }
            }
            RunEvent::BatchCompleted { id, completed_at } => {
                self.outstanding_batches = self.outstanding_batches.saturating_sub(1);
                if let Some(lag) = self.ledger.complete(id, completed_at) {
                    self.collector.record_completed_refill_lag(lag);
                }
            }
            RunEvent::Canonical(block) => self.canonical(block),
            RunEvent::Recovered { observation, tx_hashes } => {
                self.tracker.on_new_block_hashes(observation, &tx_hashes);
                self.record_confirmations();
            }
            RunEvent::Head(number) => self.tracker.observe_measurement_block(number),
            RunEvent::Receipts(receipts) => self.tracker.observe_live_receipts(&receipts),
        }
    }

    fn settle(&mut self, result: TxResult) {
        let sender = self.release_queued(result.from, result.estimated_gas);
        let record = result.measured || self.tracker.measurement() == Measurement::Warmup;
        match result.outcome {
            TxOutcome::Accepted { sent_at } => {
                self.tracker.accepted_transaction(&result.tx_hash, sent_at);
                if record {
                    self.collector.record_submitted(result.tx_hash);
                }
            }
            TxOutcome::Known => {
                if record {
                    self.collector.record_submitted(result.tx_hash);
                }
            }
            TxOutcome::Rehashed { server, sent_at } => {
                self.tracker.rehash_transaction(&result.tx_hash, server, sent_at);
                if record {
                    self.collector.record_submitted(server);
                }
            }
            TxOutcome::Failed { reason, discard } => {
                if discard {
                    self.tracker.discard_transaction(&result.tx_hash);
                }
                if record {
                    self.collector.record_failed(result.tx_hash, &reason);
                }
                if let Some(sender) = sender {
                    self.rejected_senders.push(sender);
                }
            }
        }
    }

    fn release_queued(
        &mut self,
        from: alloy_primitives::Address,
        estimated_gas: u64,
    ) -> Option<usize> {
        let sender = self.tracker.sender_index(&from);
        if let Some(sender) = sender {
            self.queued[sender] = self.queued[sender].saturating_sub(1);
        }
        self.queued_count = self.queued_count.saturating_sub(1);
        self.queued_gas = self.queued_gas.saturating_sub(u128::from(estimated_gas));
        sender
    }

    fn canonical(&mut self, block: CanonicalBlock) {
        let number = block.observation.number;
        let block_match = self.tracker.on_new_block_hashes(block.observation, &block.tx_hashes);
        self.record_confirmations();
        // A full sampler queue already holds a newer request, so this sample is skipped.
        if self.tracker.landed_in_block(number)
            && BlockWatcher::samples_receipts(number, self.tracker.observed_gas().1)
        {
            let _ = self.receipt_sampler.try_send(number);
        }
        self.log_block_match(&block, block_match);
        if self.tracker.measurement() != Measurement::Warmup {
            self.pulses.push_back(BlockPulse {
                number,
                gas_used: block.gas_used,
                gas_limit: block.gas_limit,
                base_fee: block.observation.base_fee,
                our_included_gas: block_match.included_gas,
                expected_boundary: block.expected_boundary,
                observed_at: block.observation.observed_at,
            });
        }
    }

    fn log_block_match(&mut self, block: &CanonicalBlock, block_match: BlockMatch) {
        let pending = self.tracker.pending_count();
        if block_match.matched > 0 {
            debug!(
                matched = block_match.matched,
                included_gas = block_match.included_gas,
                pending,
                block = block.observation.number,
                "block watcher matched pending transactions"
            );
            return;
        }
        if pending == 0 || self.last_unmatched_warning.elapsed() < UNMATCHED_WARNING_INTERVAL {
            return;
        }
        let (measured_pending, measured_pending_gas) =
            self.tracker.measured_unconfirmed_inventory();
        warn!(
            pending,
            in_flight = self.tracker.total_in_flight(),
            measured_pending,
            measured_pending_gas,
            latest = block.observation.number,
            latest_txs = block.tx_hashes.len(),
            latest_gas_used = block.gas_used,
            latest_gas_limit = block.gas_limit,
            "block watcher scanned block but matched no pending hashes"
        );
        self.last_unmatched_warning = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alloy_primitives::{Address, TxHash};
    use tokio::sync::{mpsc, watch};

    use super::{ProgressReport, RunCoordinator};
    use crate::{
        metrics::MetricsCollector,
        runner::{LoadTestStage, ResultsTracker, RunEvent, TxOutcome, TxResult},
    };

    const SENDER: Address = Address::repeat_byte(0x01);

    fn coordinator(
        inbox: mpsc::Receiver<RunEvent>,
        collector: &mut MetricsCollector,
    ) -> RunCoordinator<'_> {
        let (receipt_sampler, _) = mpsc::channel(1);
        let (_, base_fee) = watch::channel(0);
        RunCoordinator::new(
            inbox,
            ResultsTracker::new(&[SENDER]),
            collector,
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
        )
    }

    /// Prefill outcomes that settle after measurement starts stay out of the measured counts.
    #[tokio::test]
    async fn late_warmup_outcomes_do_not_count_toward_measurement() {
        let (events, inbox) = mpsc::channel(1);
        let mut collector = MetricsCollector::new();
        let mut coordinator = coordinator(inbox, &mut collector);
        coordinator.tracker.begin_measurement(0, None);
        let failed = |byte, measured| TxResult {
            tx_hash: TxHash::repeat_byte(byte),
            from: SENDER,
            estimated_gas: 21_000,
            measured,
            outcome: TxOutcome::Failed { reason: "rejected".into(), discard: true },
        };

        events
            .send(RunEvent::Settled(vec![failed(1, false), failed(2, true)]))
            .await
            .expect("inbox is open");
        coordinator.drain_inbox();

        assert_eq!(coordinator.collector.failed_count(), 1);
    }

    /// A wait that pumps the inbox lets a task blocked on a full inbox finish.
    #[tokio::test]
    async fn pump_until_drains_a_full_inbox_while_waiting() {
        let (events, inbox) = mpsc::channel(1);
        let mut collector = MetricsCollector::new();
        let mut coordinator = coordinator(inbox, &mut collector);
        let producer = tokio::spawn(async move {
            for number in 0..8 {
                events.send(RunEvent::Head(number)).await.expect("coordinator is alive");
            }
        });

        tokio::time::timeout(Duration::from_secs(1), coordinator.pump_until(producer))
            .await
            .expect("pumping drains the inbox")
            .expect("producer finishes");
    }
}
