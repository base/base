//! Transaction operation metrics.

use std::fmt::Debug;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

/// Metric name for the maximum possible transaction fee in gwei.
const TX_MAX_FEE_GWEI: &str = "base_tx_manager_tx_max_fee_gwei";

/// Metric name for gas bump count.
const TX_GAS_BUMP_COUNT: &str = "base_tx_manager_tx_gas_bump_count";

/// Metric name for send-loop latency in milliseconds (recorded on all exit
/// paths — both confirmed and failed transactions).
const TX_SEND_LATENCY_MS: &str = "base_tx_manager_tx_send_latency_ms";

/// Metric name for the current nonce.
const CURRENT_NONCE: &str = "base_tx_manager_current_nonce";

/// Metric name for transaction publish error count.
const TX_PUBLISH_ERROR_COUNT: &str = "base_tx_manager_tx_publish_error_count";

/// Metric name for the base fee in gwei.
const BASEFEE_GWEI: &str = "base_tx_manager_basefee_gwei";

/// Metric name for the tip cap in gwei.
const TIPCAP_GWEI: &str = "base_tx_manager_tipcap_gwei";

/// Metric name for RPC error count.
const RPC_ERROR_COUNT: &str = "base_tx_manager_rpc_error_count";

/// Metric name for confirmed transaction count.
const TX_CONFIRMED_COUNT: &str = "base_tx_manager_tx_confirmed_count";

/// Metric name for failed transaction count.
const TX_FAILED_COUNT: &str = "base_tx_manager_tx_failed_count";

/// Label key for the tx-manager instance name.
const NAME_LABEL: &str = "name";

/// Trait abstracting metrics collection for the transaction manager.
///
/// Implement this trait to plug in your own metrics backend. A [`BaseTxMetrics`]
/// implementation backed by the [`metrics`] crate is provided for production use.
pub trait TxMetrics: Send + Sync + Debug + 'static {
    /// Record the maximum possible transaction fee in gwei (`gas_limit` * `fee_cap`).
    fn record_tx_max_fee(&self, fee_gwei: f64);

    /// Record a gas bump event.
    fn record_gas_bump(&self);

    /// Record the send-loop latency in milliseconds (all exit paths).
    fn record_send_latency(&self, latency_ms: u64);

    /// Record the current nonce.
    fn record_current_nonce(&self, nonce: u64);

    /// Record a transaction publish error (transport/RPC failures only, not state errors).
    fn record_publish_error(&self);

    /// Record the base fee in gwei (fractional precision preserved).
    fn record_basefee(&self, basefee_gwei: f64);

    /// Record the tip cap in gwei (fractional precision preserved).
    fn record_tipcap(&self, tipcap_gwei: f64);

    /// Record an RPC error.
    fn record_rpc_error(&self);

    /// Record a confirmed (successfully mined) transaction.
    fn record_tx_confirmed(&self);

    /// Record a failed send attempt.
    ///
    /// This fires on **any** `send_tx` error path, including `SendTimeout`.
    /// A timeout does not guarantee the transaction will never confirm — the
    /// background `wait_for_tx` task may still be running and the transaction
    /// may already be in the mempool. This counter therefore tracks "send
    /// attempts that did not return a confirmed receipt," not definitive
    /// on-chain failures.
    fn record_tx_failed(&self);
}

/// No-op [`TxMetrics`] implementation.
///
/// All methods are no-ops — useful for unit tests and environments where
/// metrics collection is not required.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTxMetrics;

impl TxMetrics for NoopTxMetrics {
    fn record_tx_max_fee(&self, _fee_gwei: f64) {}
    fn record_gas_bump(&self) {}
    fn record_send_latency(&self, _latency_ms: u64) {}
    fn record_current_nonce(&self, _nonce: u64) {}
    fn record_publish_error(&self) {}
    fn record_basefee(&self, _basefee_gwei: f64) {}
    fn record_tipcap(&self, _tipcap_gwei: f64) {}
    fn record_rpc_error(&self) {}
    fn record_tx_confirmed(&self) {}
    fn record_tx_failed(&self) {}
}

/// Production [`TxMetrics`] implementation backed by the [`metrics`] crate.
///
/// Each method emits directly via the global metrics recorder using gauge/counter
/// macros. No handles are stored; the recorder is resolved on each call.
///
/// The `name` field is attached as a `"name"` label on every metric emission,
/// allowing multiple tx-manager instances (e.g. challenger vs. proposer) to be
/// disambiguated within a shared metrics backend.
#[derive(Debug, Clone)]
pub struct BaseTxMetrics {
    /// Instance name label attached to all metric emissions.
    pub name: &'static str,
}

impl BaseTxMetrics {
    /// Create a new [`BaseTxMetrics`] with the given instance name.
    ///
    /// The `name` is emitted as a `"name"` label on every metric, allowing
    /// multiple tx-manager instances to be distinguished in dashboards.
    pub fn new(name: &'static str) -> Self {
        Self::describe();
        Self { name }
    }

    /// Register human-readable descriptions for all tx-manager metrics.
    ///
    /// Called automatically by [`new`](Self::new). Descriptions are idempotent
    /// — calling this multiple times is safe.
    pub fn describe() {
        describe_histogram!(
            TX_MAX_FEE_GWEI,
            "Maximum possible transaction fee in gwei (gas_limit * fee_cap)"
        );
        describe_counter!(TX_GAS_BUMP_COUNT, "Number of gas bump events");
        describe_histogram!(TX_SEND_LATENCY_MS, "Send-loop latency in milliseconds");
        describe_gauge!(CURRENT_NONCE, "Current nonce value");
        describe_counter!(TX_PUBLISH_ERROR_COUNT, "Number of transaction publish errors");
        describe_gauge!(BASEFEE_GWEI, "Base fee in gwei");
        describe_gauge!(TIPCAP_GWEI, "Tip cap in gwei");
        describe_counter!(RPC_ERROR_COUNT, "Number of RPC errors");
        describe_counter!(TX_CONFIRMED_COUNT, "Number of confirmed transactions");
        describe_counter!(
            TX_FAILED_COUNT,
            "Number of failed send attempts (includes timeouts where the tx may still confirm)"
        );
    }
}

impl TxMetrics for BaseTxMetrics {
    fn record_tx_max_fee(&self, fee_gwei: f64) {
        histogram!(TX_MAX_FEE_GWEI, NAME_LABEL => self.name).record(fee_gwei);
    }

    fn record_gas_bump(&self) {
        counter!(TX_GAS_BUMP_COUNT, NAME_LABEL => self.name).increment(1);
    }

    fn record_send_latency(&self, latency_ms: u64) {
        histogram!(TX_SEND_LATENCY_MS, NAME_LABEL => self.name).record(latency_ms as f64);
    }

    fn record_current_nonce(&self, nonce: u64) {
        gauge!(CURRENT_NONCE, NAME_LABEL => self.name).set(nonce as f64);
    }

    fn record_publish_error(&self) {
        counter!(TX_PUBLISH_ERROR_COUNT, NAME_LABEL => self.name).increment(1);
    }

    fn record_basefee(&self, basefee_gwei: f64) {
        gauge!(BASEFEE_GWEI, NAME_LABEL => self.name).set(basefee_gwei);
    }

    fn record_tipcap(&self, tipcap_gwei: f64) {
        gauge!(TIPCAP_GWEI, NAME_LABEL => self.name).set(tipcap_gwei);
    }

    fn record_rpc_error(&self) {
        counter!(RPC_ERROR_COUNT, NAME_LABEL => self.name).increment(1);
    }

    fn record_tx_confirmed(&self) {
        counter!(TX_CONFIRMED_COUNT, NAME_LABEL => self.name).increment(1);
    }

    fn record_tx_failed(&self) {
        counter!(TX_FAILED_COUNT, NAME_LABEL => self.name).increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_tx_metrics_can_be_constructed_and_called() {
        let m = NoopTxMetrics;
        m.record_tx_max_fee(1.5);
        m.record_gas_bump();
        m.record_send_latency(120);
        m.record_current_nonce(42);
        m.record_publish_error();
        m.record_basefee(30.123);
        m.record_tipcap(2.456);
        m.record_rpc_error();
        m.record_tx_confirmed();
        m.record_tx_failed();
    }

    #[test]
    fn describe_is_idempotent() {
        BaseTxMetrics::describe();
        BaseTxMetrics::describe();
    }

    #[test]
    fn base_tx_metrics_can_be_constructed_and_called() {
        let m = BaseTxMetrics::new("test");
        m.record_tx_max_fee(1.5);
        m.record_gas_bump();
        m.record_send_latency(120);
        m.record_current_nonce(42);
        m.record_publish_error();
        m.record_basefee(30.123);
        m.record_tipcap(2.456);
        m.record_rpc_error();
        m.record_tx_confirmed();
        m.record_tx_failed();
    }
}
