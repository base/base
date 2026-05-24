//! Reporting helpers for devnet benchmark cycle output.

use std::collections::HashSet;

use base_zk_client::ExecutionStats;
use eyre::{Result, ensure};

/// Operation-level gas and cycle tracker metadata emitted by a benchmark workload.
#[derive(Clone, Copy, Debug)]
pub struct OperationReport {
    /// Human-readable workload operation name.
    pub operation: &'static str,
    /// Cycle tracker key emitted by the ZK program for this operation.
    pub tracker_key: &'static str,
    /// L2 block number that included the operation transaction.
    pub block_number: u64,
    /// Gas used by the operation transaction.
    pub gas_used: u64,
}

impl OperationReport {
    /// Builds an operation report from a transaction receipt.
    pub fn from_receipt(
        operation: &'static str,
        tracker_key: &'static str,
        receipt: impl alloy_network::ReceiptResponse,
    ) -> Result<Self> {
        Ok(Self {
            operation,
            tracker_key,
            block_number: receipt
                .block_number()
                .ok_or_else(|| eyre::eyre!("{operation} missing block number"))?,
            gas_used: receipt.gas_used(),
        })
    }

    /// Returns the inclusive block range covered by the operation reports.
    pub fn block_range(reports: &[Self]) -> Result<(u64, u64)> {
        let first_block = reports
            .iter()
            .map(|report| report.block_number)
            .min()
            .ok_or_else(|| eyre::eyre!("benchmark workload did not send any transactions"))?;
        let last_block = reports
            .iter()
            .map(|report| report.block_number)
            .max()
            .ok_or_else(|| eyre::eyre!("benchmark workload did not send any transactions"))?;

        Ok((first_block, last_block))
    }
}

/// Cycle report formatting helpers for devnet benchmarks.
#[derive(Debug)]
pub struct CycleReport;

impl CycleReport {
    /// Prints the benchmark summary and per-operation cycle table.
    pub fn print_summary(
        title: &str,
        first_block: u64,
        last_block: u64,
        reports: &[OperationReport],
        stats: &ExecutionStats,
    ) -> Result<()> {
        println!("{title}");
        println!("  block range:      {first_block}..={last_block}");
        println!("  transactions:     {}", reports.len());
        println!(
            "  total tx gas:     {}",
            reports.iter().map(|report| report.gas_used).sum::<u64>()
        );
        println!("  total cycles:     {}", stats.total_instruction_cycles);
        println!();
        Self::print_cycle_table(reports, stats)
    }

    /// Prints a per-operation cycle table.
    pub fn print_cycle_table(reports: &[OperationReport], stats: &ExecutionStats) -> Result<()> {
        println!("cycle tracker results");
        Self::print_table_separator();
        println!(
            "| {:<22} | {:>12} | {:>12} | {:<38} | {:>16} | {:>16} | {:>12} |",
            "operation", "block", "tx gas", "tracker key", "cycles", "cycles/call", "cycles/gas",
        );
        Self::print_table_separator();

        for report in reports {
            let tracked_cycles =
                stats.cycle_tracker.get(report.tracker_key).copied().unwrap_or_default();
            let calls_for_key =
                reports.iter().filter(|r| r.tracker_key == report.tracker_key).count() as u64;
            let cycles_per_call = tracked_cycles / calls_for_key.max(1);
            ensure!(
                tracked_cycles > 0,
                "dry-run report missing non-zero {} cycles; available keys: {:?}",
                report.tracker_key,
                stats.cycle_tracker.keys().collect::<Vec<_>>()
            );
            ensure!(report.gas_used > 0, "{} transaction reported zero gas used", report.operation);

            println!(
                "| {:<22} | {:>12} | {:>12} | {:<38} | {:>16} | {:>16} | {:>12.4} |",
                report.operation,
                report.block_number.to_string(),
                Self::fmt_u64(report.gas_used),
                report.tracker_key,
                Self::fmt_u64(tracked_cycles),
                Self::fmt_u64(cycles_per_call),
                cycles_per_call as f64 / report.gas_used as f64,
            );
        }

        Self::print_table_separator();
        let unique_tracked_cycles = {
            let mut seen = HashSet::new();
            reports
                .iter()
                .filter(|report| seen.insert(report.tracker_key))
                .map(|report| {
                    stats.cycle_tracker.get(report.tracker_key).copied().unwrap_or_default()
                })
                .sum::<u64>()
        };
        println!("tracked cycles: {}", Self::fmt_u64(unique_tracked_cycles));

        Ok(())
    }

    /// Prints the fixed-width cycle table separator.
    pub fn print_table_separator() {
        println!(
            "+-{:-<22}-+-{:-<12}-+-{:-<12}-+-{:-<38}-+-{:-<16}-+-{:-<16}-+-{:-<12}-+",
            "", "", "", "", "", "", "",
        )
    }

    /// Formats an integer with comma group separators.
    pub fn fmt_u64(value: u64) -> String {
        let digits = value.to_string();
        let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
        for (idx, ch) in digits.chars().rev().enumerate() {
            if idx > 0 && idx % 3 == 0 {
                formatted.push(',');
            }
            formatted.push(ch);
        }
        formatted.chars().rev().collect()
    }

    /// Formats a duration for compact benchmark progress output.
    pub fn fmt_duration(duration: std::time::Duration) -> String {
        let seconds = duration.as_secs();
        let minutes = seconds / 60;
        let seconds = seconds % 60;
        if minutes > 0 { format!("{minutes}m {seconds:02}s") } else { format!("{seconds}s") }
    }
}
