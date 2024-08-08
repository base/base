use std::fmt;

use num_format::{Locale, ToFormattedString};

/// Statistics for the multi-block execution.
#[derive(Debug)]
pub struct ExecutionStats {
    pub total_instruction_count: u64,
    pub block_execution_instruction_count: u64,
    pub nb_blocks: u64,
    pub nb_transactions: u64,
    pub total_gas_used: u64,
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
