//! Transaction operation metrics.

use std::fmt::Debug;

use metrics::{counter, gauge, histogram};

/// Metric name for the transaction fee in gwei.
const TX_FEE_GWEI: &str = "base_tx_manager_tx_fee_gwei";

/// Metric name for gas bump count.
const TX_GAS_BUMP: &str = "base_tx_manager_tx_gas_bump";

/// Metric name for confirmed transaction latency in milliseconds.
const TX_CONFIRMED_LATENCY_MS: &str = "base_tx_manager_tx_confirmed_latency_ms";

/// Metric name for the current nonce.
const CURRENT_NONCE: &str = "base_tx_manager_current_nonce";

/// Metric name for transaction publish error count.
const TX_PUBLISH_ERROR_COUNT: &str = "base_tx_manager_tx_publish_error_count";

/// Metric name for the base fee in wei.
const BASEFEE_WEI: &str = "base_tx_manager_basefee_wei";

/// Metric name for the tip cap in wei.
const TIPCAP_WEI: &str = "base_tx_manager_tipcap_wei";

/// Metric name for RPC error count.
const RPC_ERROR_COUNT: &str = "base_tx_manager_rpc_error_count";

/// Trait abstracting metrics collection for the transaction manager.
///
/// Implement this trait to plug in your own metrics backend. A [`BaseTxMetrics`]
/// implementation backed by the [`metrics`] crate is provided for production use.
pub trait TxMetrics: Send + Sync + Debug + 'static {
    /// Record the transaction fee in gwei.
    fn record_tx_fee(&self, fee_gwei: f64);

    /// Record a gas bump event.
    fn record_gas_bump(&self);

    /// Record the confirmed transaction latency in milliseconds.
    fn record_confirmed_latency(&self, latency_ms: f64);

    /// Record the current nonce.
    fn record_current_nonce(&self, nonce: u64);

    /// Record a transaction publish error.
    fn record_publish_error(&self);

    /// Record the base fee in wei.
    fn record_basefee(&self, basefee_wei: f64);

    /// Record the tip cap in wei.
    fn record_tipcap(&self, tipcap_wei: f64);

    /// Record an RPC error.
    fn record_rpc_error(&self);
}

/// No-op [`TxMetrics`] implementation.
///
/// All methods are no-ops — useful for unit tests and environments where
/// metrics collection is not required.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTxMetrics;

impl TxMetrics for NoopTxMetrics {
    fn record_tx_fee(&self, _fee_gwei: f64) {}
    fn record_gas_bump(&self) {}
    fn record_confirmed_latency(&self, _latency_ms: f64) {}
    fn record_current_nonce(&self, _nonce: u64) {}
    fn record_publish_error(&self) {}
    fn record_basefee(&self, _basefee_wei: f64) {}
    fn record_tipcap(&self, _tipcap_wei: f64) {}
    fn record_rpc_error(&self) {}
}

/// Production [`TxMetrics`] implementation backed by the [`metrics`] crate.
///
/// Each method emits directly via the global metrics recorder using gauge/counter
/// macros. No handles are stored; the recorder is resolved on each call.
#[derive(Debug, Clone, Copy, Default)]
pub struct BaseTxMetrics;

impl TxMetrics for BaseTxMetrics {
    fn record_tx_fee(&self, fee_gwei: f64) {
        gauge!(TX_FEE_GWEI).set(fee_gwei);
    }

    fn record_gas_bump(&self) {
        counter!(TX_GAS_BUMP).increment(1);
    }

    fn record_confirmed_latency(&self, latency_ms: f64) {
        histogram!(TX_CONFIRMED_LATENCY_MS).record(latency_ms);
    }

    #[allow(clippy::cast_precision_loss)]
    fn record_current_nonce(&self, nonce: u64) {
        gauge!(CURRENT_NONCE).set(nonce as f64);
    }

    fn record_publish_error(&self) {
        counter!(TX_PUBLISH_ERROR_COUNT).increment(1);
    }

    fn record_basefee(&self, basefee_wei: f64) {
        gauge!(BASEFEE_WEI).set(basefee_wei);
    }

    fn record_tipcap(&self, tipcap_wei: f64) {
        gauge!(TIPCAP_WEI).set(tipcap_wei);
    }

    fn record_rpc_error(&self) {
        counter!(RPC_ERROR_COUNT).increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_tx_metrics_can_be_constructed_and_called() {
        let m = NoopTxMetrics;
        m.record_tx_fee(1.5);
        m.record_gas_bump();
        m.record_confirmed_latency(120.0);
        m.record_current_nonce(42);
        m.record_publish_error();
        m.record_basefee(1_000_000.0);
        m.record_tipcap(500_000.0);
        m.record_rpc_error();
    }

    #[test]
    fn base_tx_metrics_can_be_constructed_and_called() {
        let m = BaseTxMetrics;
        m.record_tx_fee(1.5);
        m.record_gas_bump();
        m.record_confirmed_latency(120.0);
        m.record_current_nonce(42);
        m.record_publish_error();
        m.record_basefee(1_000_000.0);
        m.record_tipcap(500_000.0);
        m.record_rpc_error();
    }
}
