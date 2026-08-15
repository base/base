//! Progress display helpers for system benchmarks.

use std::time::{Duration, Instant};

use alloy_primitives::Address;
use base_prover_service_protocol::{ExecutionStats, ProofStatus};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use super::{CycleReport, OperationReport};

/// Multi-line terminal progress display for long-running system benchmarks.
#[derive(Debug)]
pub struct BenchDisplay {
    benchmark_name: &'static str,
    _multi: MultiProgress,
    header: ProgressBar,
    setup: ProgressBar,
    txs: ProgressBar,
    safe_l2: ProgressBar,
    proof: ProgressBar,
    started_at: Instant,
}

impl BenchDisplay {
    /// Creates a display with setup, transaction, safe L2, and proof progress rows.
    pub fn new(benchmark_name: &'static str, total_txs: u64) -> Self {
        let multi = MultiProgress::new();
        let header = multi.add(ProgressBar::new_spinner());
        header.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("template is valid"),
        );
        header.set_message(format!("{benchmark_name} starting..."));
        header.enable_steady_tick(Duration::from_millis(120));

        let spinner_style =
            ProgressStyle::with_template("  {spinner:.cyan} {msg}").expect("template is valid");
        let setup = Self::spinner(&multi, &spinner_style, "setup waiting for benchmark accounts");
        let safe_l2 = Self::spinner(&multi, &spinner_style, "safe L2 waiting for workload blocks");
        let proof = Self::spinner(&multi, &spinner_style, "prover waiting for dry-run job");

        let txs = multi.add(ProgressBar::new(total_txs));
        txs.set_style(
            ProgressStyle::with_template("  txs   [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                .expect("template is valid")
                .progress_chars("#>-"),
        );
        txs.set_message("pending workload");

        Self {
            benchmark_name,
            _multi: multi,
            header,
            setup,
            txs,
            safe_l2,
            proof,
            started_at: Instant::now(),
        }
    }

    /// Creates a spinner progress row with the given style and initial message.
    pub fn spinner(
        multi: &MultiProgress,
        style: &ProgressStyle,
        message: &'static str,
    ) -> ProgressBar {
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(style.clone());
        pb.set_message(message);
        pb.enable_steady_tick(Duration::from_millis(120));
        pb
    }

    /// Updates the setup progress row.
    pub fn setup_message(&self, message: impl Into<String>) {
        self.setup.set_message(message.into());
    }

    /// Marks setup complete with the created workload target.
    pub fn setup_done(&self, target: Address) {
        self.setup.finish_with_message(format!("setup ready   target {target}"));
    }

    /// Updates the transaction row before sending a workload operation.
    pub fn tx_started(&self, operation: &str) {
        self.header.set_message(format!("{} sending {operation}", self.benchmark_name));
        self.txs.set_message(format!("sending {operation}"));
    }

    /// Records a completed workload transaction.
    pub fn tx_done(&self, report: &OperationReport) {
        self.txs.inc(1);
        self.txs.set_message(format!(
            "last {}   gas {}   block {}",
            report.operation,
            CycleReport::fmt_u64(report.gas_used),
            report.block_number,
        ));
    }

    /// Marks all workload transactions as included.
    pub fn txs_done(&self) {
        self.txs.finish_with_message("workload transactions included");
    }

    /// Updates safe L2 wait progress.
    pub fn safe_l2_progress(&self, safe_block: u64, target_block: u64) {
        self.header.set_message("waiting for safe L2");
        self.safe_l2.set_message(format!("safe L2 {safe_block} / target {target_block}"));
    }

    /// Marks the safe L2 wait complete.
    pub fn safe_l2_done(&self, block_number: u64) {
        self.safe_l2.finish_with_message(format!("safe L2 reached block {block_number}"));
    }

    /// Records that a proof request has been accepted.
    pub fn proof_requested(&self, session_id: &str, start_block: u64, blocks: u64) {
        self.header.set_message("dry-run proof in progress");
        self.proof.set_message(format!(
            "session {session_id}   range start {start_block}   blocks {blocks}",
        ));
    }

    /// Updates proof polling progress.
    pub fn proof_progress(&self, status: &ProofStatus, elapsed: Duration) {
        self.proof.set_message(format!(
            "status {status:?}   elapsed {}",
            CycleReport::fmt_duration(elapsed)
        ));
    }

    /// Marks proof polling complete.
    pub fn proof_done(&self, stats: &ExecutionStats) {
        self.proof.finish_with_message(format!(
            "dry-run complete   total cycles {}",
            CycleReport::fmt_u64(stats.total_instruction_cycles),
        ));
        self.header.finish_with_message(format!(
            "{} complete in {}",
            self.benchmark_name,
            CycleReport::fmt_duration(self.started_at.elapsed()),
        ));
    }
}
