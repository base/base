//! Gas price statistics collector.

use reth_transaction_pool::TransactionPool;

use crate::types::ProcessedData;

/// Collector for gas price statistics from the transaction pool.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct GasCollector {
    /// Last gas limit observed.
    last_gas_limit: u64,
}

impl GasCollector {
    /// Creates a new gas collector.
    pub(crate) const fn new() -> Self {
        Self { last_gas_limit: 0 }
    }

    /// Updates the last observed gas limit.
    #[allow(dead_code)]
    pub(crate) const fn set_gas_limit(&mut self, gas_limit: u64) {
        self.last_gas_limit = gas_limit;
    }

    /// Collects gas price statistics from the transaction pool.
    /// Returns values in gwei for the frontend.
    #[allow(dead_code)]
    pub(crate) fn collect<P: TransactionPool>(&self, pool: &P) -> ProcessedData {
        let pending = pool.pending_transactions();
        let mut prices: Vec<u128> = pending.iter().map(|tx| tx.max_fee_per_gas()).collect();

        if prices.is_empty() {
            return ProcessedData { gas_limit: self.last_gas_limit, ..Default::default() };
        }

        prices.sort_unstable();
        let count = prices.len();

        let min = prices.first().copied().unwrap_or(0);
        let max = prices.last().copied().unwrap_or(0);
        let sum: u128 = prices.iter().sum();
        let avg = sum / count as u128;
        let median = if count.is_multiple_of(2) {
            (prices[count / 2 - 1] + prices[count / 2]) / 2
        } else {
            prices[count / 2]
        };

        // Convert from wei to gwei (divide by 1e9)
        let to_gwei = |wei: u128| -> f64 { wei as f64 / 1_000_000_000.0 };

        ProcessedData {
            min_gas: to_gwei(min),
            median_gas: to_gwei(median),
            ave_gas: to_gwei(avg),
            max_gas: to_gwei(max),
            gas_limit: self.last_gas_limit,
            ..Default::default()
        }
    }
}
