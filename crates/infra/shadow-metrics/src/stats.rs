//! Statistics derived from persisted shadow block payloads.

use alloy_consensus::Transaction;
use base_shadow_indexer_db::ShadowBlockPayload;

/// Statistics derived from one persisted shadow candidate block.
///
/// Deposit transactions remain part of block-wide totals but are excluded from fee ordering.
/// They execute before the flashblock loop, always occupy the first positions, and are not
/// fee-ordered, so including them would create a phantom inversion at the deposit/user boundary.
#[derive(Clone, Debug)]
pub struct ShadowBlockStats {
    /// Block number from the persisted block header.
    pub number: u64,
    /// Total gas used from the block header, including deposit transactions.
    pub gas_used: u64,
    /// Total transactions in the block body, including deposit transactions.
    pub transaction_count: usize,
    /// Number of non-deposit transactions, used to identify empty shadow blocks.
    pub non_deposit_tx_count: usize,
    /// Strictly increasing adjacent effective-tip pairs among non-deposit transactions.
    ///
    /// Equal adjacent tips are not inversions. This matches
    /// `builder/core/tests/ordering.rs::fee_priority_ordering`, whose equivalent predicate is
    /// `pair[0] < pair[1]`, so the metric and builder assertion agree by construction.
    /// Non-zero values are expected: each flashblock refresh restarts transaction selection at
    /// the pool's current highest tip, producing a sawtooth fee curve with one descending run per
    /// flashblock. The baseline therefore equals flashblocks per block, but that deployment value
    /// is intentionally not encoded in this type.
    pub priority_fee_inversions: usize,
    /// Builder version stamped onto the persisted payload by `ShadowWriter::stamp_row`.
    pub builder_version: String,
}

impl ShadowBlockStats {
    /// Derives reader-emitted statistics from one persisted shadow block payload.
    ///
    /// Deposit transactions are omitted from the fee vector because they are not fee-ordered.
    /// Transactions whose `effective_tip_per_gas` returns `None` have `max_fee < base_fee`, which
    /// is invalid in a mined block. Such tips are omitted rather than represented as zero, which
    /// would fabricate an inversion. Inversions use a strictly greater next tip, matching the
    /// builder's fee-priority ordering assertion.
    #[must_use]
    pub fn from_payload(payload: &ShadowBlockPayload) -> Self {
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
            number: header.number,
            gas_used: header.gas_used,
            transaction_count: transactions.len(),
            non_deposit_tx_count,
            priority_fee_inversions,
            builder_version: payload.builder_version.clone(),
        }
    }
}
