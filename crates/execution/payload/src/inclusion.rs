//! Per-block accounting of transaction inclusion and fee revenue.

use alloy_primitives::U256;

/// Metric `flow` label for validity-transaction fee revenue.
pub const FLOW_VALIDITY: &str = "validity";
/// Metric `flow` label for standard (non-validity) fee revenue.
pub const FLOW_STANDARD: &str = "standard";

/// Per-flow inclusion and EIP-1559 fee totals for one block.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InclusionFlow {
    /// Transactions committed to the payload in this flow.
    pub txs: u64,
    /// Gas consumed by those transactions.
    pub gas: u64,
    /// Base-fee revenue (`base_fee * gas_used`), in wei.
    pub base_fees: U256,
    /// Priority-fee revenue (`effective_tip * gas_used`), in wei.
    pub priority_fees: U256,
    /// Coinbase-tip revenue from EIP-8130 phase-0 bids, in wei.
    pub coinbase_tips: U256,
}

impl InclusionFlow {
    fn record(&mut self, gas_used: u64, miner_fee: u128, base_fee: u64, coinbase_tip: U256) {
        self.txs += 1;
        self.gas += gas_used;
        self.base_fees += U256::from(base_fee) * U256::from(gas_used);
        self.priority_fees += U256::from(miner_fee) * U256::from(gas_used);
        self.coinbase_tips += coinbase_tip;
    }

    /// Base-fee revenue as `f64` wei for histogram emission.
    pub fn base_fees_f64(&self) -> f64 {
        u128::try_from(self.base_fees).map(|wei| wei as f64).unwrap_or(f64::MAX)
    }

    /// Priority-fee revenue as `f64` wei for histogram emission.
    pub fn priority_fees_f64(&self) -> f64 {
        u128::try_from(self.priority_fees).map(|wei| wei as f64).unwrap_or(f64::MAX)
    }

    /// Coinbase-tip revenue as `f64` wei for histogram emission.
    pub fn coinbase_tips_f64(&self) -> f64 {
        u128::try_from(self.coinbase_tips).map(|wei| wei as f64).unwrap_or(f64::MAX)
    }
}

/// Per-block accumulator for inclusion and EIP-1559 fee revenue, segmented by flow.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InclusionTracker {
    /// Standard (non-validity) mempool transactions.
    pub standard: InclusionFlow,
    /// Validity-gated mempool transactions.
    pub validity: InclusionFlow,
}

impl InclusionTracker {
    /// Records a mempool transaction that was committed to the payload.
    ///
    /// `is_validity` selects the [`Self::validity`] or [`Self::standard`] flow.
    /// `coinbase_tip` is the statically-decoded EIP-8130 phase-0 tip
    /// (`base_common_consensus::CoinbaseTip::decode`), or [`U256::ZERO`] when
    /// the transaction does not carry one.
    pub fn record(
        &mut self,
        is_validity: bool,
        gas_used: u64,
        miner_fee: u128,
        base_fee: u64,
        coinbase_tip: U256,
    ) {
        let flow = if is_validity { &mut self.validity } else { &mut self.standard };
        flow.record(gas_used, miner_fee, base_fee, coinbase_tip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_validity_inclusion_gas_and_fees() {
        let mut tracker = InclusionTracker::default();
        tracker.record(true, 21_000, 2, 10, U256::ZERO);

        assert_eq!(tracker.validity.txs, 1);
        assert_eq!(tracker.validity.gas, 21_000);
        assert_eq!(tracker.validity.priority_fees_f64(), 42_000.0);
        assert_eq!(tracker.validity.base_fees_f64(), 210_000.0);
        assert_eq!(tracker.validity.coinbase_tips_f64(), 0.0);
        assert_eq!(tracker.standard, InclusionFlow::default());
    }

    #[test]
    fn records_standard_fees_without_validity_counts() {
        let mut tracker = InclusionTracker::default();
        tracker.record(false, 30_000, 3, 10, U256::ZERO);

        assert_eq!(tracker.validity, InclusionFlow::default());
        assert_eq!(tracker.standard.txs, 1);
        assert_eq!(tracker.standard.gas, 30_000);
        assert_eq!(tracker.standard.priority_fees_f64(), 90_000.0);
        assert_eq!(tracker.standard.base_fees_f64(), 300_000.0);
        assert_eq!(tracker.standard.coinbase_tips_f64(), 0.0);
    }

    #[test]
    fn records_coinbase_tip_revenue_per_flow() {
        let mut tracker = InclusionTracker::default();
        tracker.record(true, 21_000, 0, 10, U256::from(500));
        tracker.record(true, 8_000, 0, 10, U256::from(250));
        tracker.record(false, 21_000, 0, 10, U256::from(100));

        assert_eq!(tracker.validity.coinbase_tips_f64(), 750.0);
        assert_eq!(tracker.standard.coinbase_tips_f64(), 100.0);
    }

    #[test]
    fn accumulates_mixed_mempool_transactions_across_the_block() {
        let mut tracker = InclusionTracker::default();
        tracker.record(true, 21_000, 2, 10, U256::ZERO);
        tracker.record(false, 10_000, 4, 10, U256::ZERO);
        tracker.record(true, 9_000, 1, 10, U256::ZERO);
        tracker.record(true, 8_000, 9, 10, U256::ZERO);

        assert_eq!(tracker.validity.txs, 3);
        assert_eq!(tracker.validity.gas, 38_000);
        assert_eq!(tracker.validity.priority_fees_f64(), 123_000.0);
        assert_eq!(tracker.validity.base_fees_f64(), 380_000.0);
        assert_eq!(tracker.validity.coinbase_tips_f64(), 0.0);
        assert_eq!(tracker.standard.txs, 1);
        assert_eq!(tracker.standard.gas, 10_000);
        assert_eq!(tracker.standard.priority_fees_f64(), 40_000.0);
        assert_eq!(tracker.standard.base_fees_f64(), 100_000.0);
        assert_eq!(tracker.standard.coinbase_tips_f64(), 0.0);
    }
}
