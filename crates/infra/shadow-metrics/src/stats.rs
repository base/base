//! Statistics derived from persisted shadow blocks.

use alloy_consensus::Transaction;
use base_shadow_indexer_db::ShadowBlockRow;

/// Statistics for one shadow candidate block.
#[derive(Clone, Debug)]
pub struct ShadowBlockStats {
    /// Persisted block number.
    pub number: i64,
    /// Header gas used, including deposits.
    pub gas_used: u64,
    /// Transaction count, including deposits.
    pub transaction_count: usize,
    /// Non-deposit transaction count.
    pub non_deposit_tx_count: usize,
    /// Adjacent effective-tip increases.
    pub priority_fee_inversions: usize,
    /// Writer-stamped builder version.
    pub builder_version: String,
}

impl ShadowBlockStats {
    /// Derives metrics from a persisted row.
    ///
    /// Deposits stay in totals but leave the fee vector because they are not fee-ordered.
    /// Missing tips are skipped, not zeroed; zero would invent an inversion.
    /// Strict `next > previous` matches the builder assertion and excludes equal tips.
    #[must_use]
    pub fn from_row(row: &ShadowBlockRow) -> Self {
        let payload = &row.payload;
        let block = &payload.block;
        let header = block.header();
        let transactions = &block.body().transactions;
        let base_fee = header.base_fee_per_gas.unwrap_or_default();
        let tips: Vec<u128> = transactions
            .iter()
            .filter(|tx| !tx.is_deposit())
            .filter_map(|tx| tx.effective_tip_per_gas(base_fee))
            .collect();

        let non_deposit_tx_count = transactions.iter().filter(|tx| !tx.is_deposit()).count();
        let priority_fee_inversions =
            tips.windows(2).filter(|window| window[1] > window[0]).count();

        Self {
            number: row.number,
            gas_used: header.gas_used,
            transaction_count: transactions.len(),
            non_deposit_tx_count,
            priority_fee_inversions,
            builder_version: payload.builder_version.clone(),
        }
    }
}
