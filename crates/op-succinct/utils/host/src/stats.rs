use std::{fmt, time::Duration};

use crate::fetcher::{ChainMode, OPSuccinctDataFetcher};
use num_format::{Locale, ToFormattedString};
use serde::{Deserialize, Serialize};
use sp1_sdk::{CostEstimator, ExecutionReport};

/// Statistics for the multi-block execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStats {
    pub batch_start: u64,
    pub batch_end: u64,
    pub execution_duration_sec: u64,
    pub total_instruction_count: u64,
    pub oracle_verify_instruction_count: u64,
    pub derivation_instruction_count: u64,
    pub block_execution_instruction_count: u64,
    pub blob_verification_instruction_count: u64,
    pub total_sp1_gas: u64,
    pub nb_blocks: u64,
    pub nb_transactions: u64,
    pub eth_gas_used: u64,
    pub cycles_per_block: u64,
    pub cycles_per_transaction: u64,
    pub transactions_per_block: u64,
    pub gas_used_per_block: u64,
    pub gas_used_per_transaction: u64,
    pub bn_pair_cycles: u64,
    pub bn_add_cycles: u64,
    pub bn_mul_cycles: u64,
    pub kzg_eval_cycles: u64,
    pub ec_recover_cycles: u64,
}

/// Write a statistic to the formatter.
fn write_stat(f: &mut fmt::Formatter<'_>, label: &str, value: u64) -> fmt::Result {
    writeln!(f, "| {:<30} | {:>25} |", label, value.to_formatted_string(&Locale::en))
}

impl fmt::Display for ExecutionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "+--------------------------------+---------------------------+")?;
        writeln!(f, "| {:<30} | {:<25} |", "Metric", "Value")?;
        writeln!(f, "+--------------------------------+---------------------------+")?;
        write_stat(f, "Batch Start", self.batch_start)?;
        write_stat(f, "Batch End", self.batch_end)?;
        write_stat(f, "Execution Duration (seconds)", self.execution_duration_sec)?;
        write_stat(f, "Total Instruction Count", self.total_instruction_count)?;
        write_stat(f, "Oracle Verify Cycles", self.oracle_verify_instruction_count)?;
        write_stat(f, "Derivation Cycles", self.derivation_instruction_count)?;
        write_stat(f, "Block Execution Cycles", self.block_execution_instruction_count)?;
        write_stat(f, "Blob Verification Cycles", self.blob_verification_instruction_count)?;
        write_stat(f, "Total SP1 Gas", self.total_sp1_gas)?;
        write_stat(f, "Number of Blocks", self.nb_blocks)?;
        write_stat(f, "Number of Transactions", self.nb_transactions)?;
        write_stat(f, "Ethereum Gas Used", self.eth_gas_used)?;
        write_stat(f, "Cycles per Block", self.cycles_per_block)?;
        write_stat(f, "Cycles per Transaction", self.cycles_per_transaction)?;
        write_stat(f, "Transactions per Block", self.transactions_per_block)?;
        write_stat(f, "Gas Used per Block", self.gas_used_per_block)?;
        write_stat(f, "Gas Used per Transaction", self.gas_used_per_transaction)?;
        write_stat(f, "BN Pair Cycles", self.bn_pair_cycles)?;
        write_stat(f, "BN Add Cycles", self.bn_add_cycles)?;
        write_stat(f, "BN Mul Cycles", self.bn_mul_cycles)?;
        write_stat(f, "KZG Eval Cycles", self.kzg_eval_cycles)?;
        write_stat(f, "EC Recover Cycles", self.ec_recover_cycles)?;
        writeln!(f, "+--------------------------------+---------------------------+")
    }
}

/// Get the execution stats for a given report.
pub async fn get_execution_stats(
    data_fetcher: &OPSuccinctDataFetcher,
    start: u64,
    end: u64,
    report: &ExecutionReport,
    execution_duration: Duration,
) -> ExecutionStats {
    // Get the total instruction count for execution across all blocks.
    let block_execution_instruction_count: u64 =
        *report.cycle_tracker.get("block-execution").unwrap_or(&0);
    let oracle_verify_instruction_count: u64 =
        *report.cycle_tracker.get("oracle-verify").unwrap_or(&0);
    let derivation_instruction_count: u64 =
        *report.cycle_tracker.get("payload-derivation").unwrap_or(&0);
    let blob_verification_instruction_count: u64 =
        *report.cycle_tracker.get("blob-verification").unwrap_or(&0);

    let nb_blocks = end - start + 1;

    // Fetch the number of transactions in the blocks from the L2 RPC.
    let block_data_range = data_fetcher
        .get_block_data_range(ChainMode::L2, start, end)
        .await
        .expect("Failed to fetch block data range.");

    let nb_transactions = block_data_range.iter().map(|b| b.transaction_count).sum();
    let total_gas_used = block_data_range.iter().map(|b| b.gas_used).sum();

    let bn_add_cycles: u64 = *report.cycle_tracker.get("precompile-bn-add").unwrap_or(&0);
    let bn_mul_cycles: u64 = *report.cycle_tracker.get("precompile-bn-mul").unwrap_or(&0);
    let bn_pair_cycles: u64 = *report.cycle_tracker.get("precompile-bn-pair").unwrap_or(&0);
    let kzg_eval_cycles: u64 = *report.cycle_tracker.get("precompile-kzg-eval").unwrap_or(&0);
    let ec_recover_cycles: u64 = *report.cycle_tracker.get("precompile-ec-recover").unwrap_or(&0);

    let cycles_per_block = block_execution_instruction_count / nb_blocks;
    let cycles_per_transaction = block_execution_instruction_count / nb_transactions;
    let transactions_per_block = nb_transactions / nb_blocks;
    let gas_used_per_block = total_gas_used / nb_blocks;
    let gas_used_per_transaction = total_gas_used / nb_transactions;

    ExecutionStats {
        batch_start: start,
        batch_end: end,
        execution_duration_sec: execution_duration.as_secs(),
        total_instruction_count: report.total_instruction_count(),
        derivation_instruction_count,
        oracle_verify_instruction_count,
        block_execution_instruction_count,
        blob_verification_instruction_count,
        total_sp1_gas: report.estimate_gas(),
        nb_blocks,
        nb_transactions,
        eth_gas_used: total_gas_used,
        cycles_per_block,
        cycles_per_transaction,
        transactions_per_block,
        gas_used_per_block,
        gas_used_per_transaction,
        bn_add_cycles,
        bn_mul_cycles,
        bn_pair_cycles,
        kzg_eval_cycles,
        ec_recover_cycles,
    }
}

#[derive(Debug, Clone)]
pub struct SpanBatchStats {
    pub span_start: u64,
    pub span_end: u64,
    pub total_blocks: u64,
    pub total_transactions: u64,
    pub total_gas_used: u64,
    pub total_cycles: u64,
    pub total_sp1_gas: u64,
    pub cycles_per_block: u64,
    pub cycles_per_transaction: u64,
    pub gas_used_per_block: u64,
    pub gas_used_per_transaction: u64,
    pub total_derivation_cycles: u64,
    pub total_execution_cycles: u64,
    pub total_blob_verification_cycles: u64,
    pub bn_add_cycles: u64,
    pub bn_mul_cycles: u64,
    pub bn_pair_cycles: u64,
    pub kzg_eval_cycles: u64,
    pub ec_recover_cycles: u64,
}

impl fmt::Display for SpanBatchStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "+-------------------------------+---------------------------+")?;
        writeln!(f, "| {:<30} | {:<25} |", "Metric", "Value")?;
        writeln!(f, "+-------------------------------+---------------------------+")?;
        write_stat(f, "Span Start", self.span_start)?;
        write_stat(f, "Span End", self.span_end)?;
        write_stat(f, "Total Blocks", self.total_blocks)?;
        write_stat(f, "Total Transactions", self.total_transactions)?;
        write_stat(f, "Total Gas Used", self.total_gas_used)?;
        write_stat(f, "Total Cycles", self.total_cycles)?;
        write_stat(f, "Total SP1 Gas", self.total_sp1_gas)?;
        write_stat(f, "Cycles per Block", self.cycles_per_block)?;
        write_stat(f, "Cycles per Transaction", self.cycles_per_transaction)?;
        write_stat(f, "Gas Used per Block", self.gas_used_per_block)?;
        write_stat(f, "Gas Used per Transaction", self.gas_used_per_transaction)?;
        write_stat(f, "Total Derivation Cycles", self.total_derivation_cycles)?;
        write_stat(f, "Total Execution Cycles", self.total_execution_cycles)?;
        write_stat(f, "Total Blob Verification Cycles", self.total_blob_verification_cycles)?;
        write_stat(f, "BN Add Cycles", self.bn_add_cycles)?;
        write_stat(f, "BN Mul Cycles", self.bn_mul_cycles)?;
        write_stat(f, "BN Pair Cycles", self.bn_pair_cycles)?;
        write_stat(f, "KZG Eval Cycles", self.kzg_eval_cycles)?;
        write_stat(f, "EC Recover Cycles", self.ec_recover_cycles)?;
        writeln!(f, "+-------------------------------+---------------------------+")
    }
}
