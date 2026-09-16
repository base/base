//! Statistics derived from persisted shadow blocks.

use alloy_consensus::Header;
use base_block_stats::BlockStats;
use base_common_consensus::BaseTxEnvelope;
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
        let block = &row.payload.block;
        Self::from_parts(
            row.number,
            row.payload.builder_version.clone(),
            block.header(),
            &block.body().transactions,
        )
    }

    /// Derives metrics from a header and transaction list, without a full row.
    ///
    /// Delegates the per-block figures to [`BlockStats::from_transactions`].
    #[must_use]
    pub fn from_parts(
        number: i64,
        builder_version: String,
        header: &Header,
        transactions: &[BaseTxEnvelope],
    ) -> Self {
        let stats = BlockStats::from_transactions(
            header.gas_used,
            header.base_fee_per_gas.unwrap_or_default(),
            transactions,
        );
        Self {
            number,
            gas_used: stats.gas_used,
            transaction_count: stats.transaction_count,
            non_deposit_tx_count: stats.non_deposit_transaction_count,
            priority_fee_inversions: stats.priority_fee_inversions,
            builder_version,
        }
    }
}
