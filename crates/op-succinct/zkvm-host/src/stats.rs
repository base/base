use std::fmt;

use num_format::{Locale, ToFormattedString};

#[derive(Debug)]
pub struct BnStats {
    pub bn_pair_cycles: u64,
    pub bn_add_cycles: u64,
    pub bn_mul_cycles: u64,
}

/// Statistics for the multi-block execution.
#[derive(Debug)]
pub struct ExecutionStats {
    pub total_instruction_count: u64,
    pub block_execution_instruction_count: u64,
    pub nb_blocks: u64,
    pub nb_transactions: u64,
    pub total_gas_used: u64,
    pub bn_stats: BnStats,
}

/// Write a statistic to the formatter.
fn write_stat(f: &mut fmt::Formatter<'_>, label: &str, value: u64) -> fmt::Result {
    writeln!(
        f,
        "| {:<30} | {:>25} |",
        label,
        value.to_formatted_string(&Locale::en)
    )
}

impl fmt::Display for ExecutionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cycles_per_block = self.block_execution_instruction_count / self.nb_blocks;
        let cycles_per_transaction = self.block_execution_instruction_count / self.nb_transactions;
        let transactions_per_block = self.nb_transactions / self.nb_blocks;
        let gas_used_per_block = self.total_gas_used / self.nb_blocks;
        let gas_used_per_transaction = self.total_gas_used / self.nb_transactions;

        writeln!(
            f,
            "+--------------------------------+---------------------------+"
        )?;
        writeln!(f, "| {:<30} | {:<25} |", "Metric", "Value")?;
        writeln!(
            f,
            "+--------------------------------+---------------------------+"
        )?;
        write_stat(f, "Total Cycles", self.total_instruction_count)?;
        write_stat(
            f,
            "Block Execution Cycles",
            self.block_execution_instruction_count,
        )?;
        write_stat(f, "Bn Pair Cycles", self.bn_stats.bn_pair_cycles)?;
        write_stat(f, "Bn Add Cycles", self.bn_stats.bn_add_cycles)?;
        write_stat(f, "Bn Mul Cycles", self.bn_stats.bn_mul_cycles)?;
        write_stat(f, "Total Blocks", self.nb_blocks)?;
        write_stat(f, "Total Transactions", self.nb_transactions)?;
        write_stat(f, "Cycles per Block", cycles_per_block)?;
        write_stat(f, "Cycles per Transaction", cycles_per_transaction)?;
        write_stat(f, "Transactions per Block", transactions_per_block)?;
        write_stat(f, "Total Gas Used", self.total_gas_used)?;
        write_stat(f, "Gas Used per Block", gas_used_per_block)?;
        write_stat(f, "Gas Used per Transaction", gas_used_per_transaction)?;
        writeln!(
            f,
            "+--------------------------------+---------------------------+"
        )
    }
}
