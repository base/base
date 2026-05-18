//! Transaction operation metrics.

use std::fmt::Debug;

base_metrics::define_metrics! {
    base_tx_manager,
    struct = TxManagerMetrics,
    #[describe("Maximum possible transaction fee in gwei")]
    #[label(name)]
    tx_max_fee_gwei: histogram,
    #[describe("Number of gas bump events")]
    #[label(name)]
    tx_gas_bump_count: counter,
    #[describe("Send-loop latency in milliseconds")]
    #[label(name)]
    tx_send_latency_ms: histogram,
    #[describe("Current nonce value")]
    #[label(name)]
    current_nonce: gauge,
    #[describe("Number of transaction publish errors")]
    #[label(name)]
    tx_publish_error_count: counter,
    #[describe("Base fee in gwei")]
    #[label(name)]
    basefee_gwei: gauge,
    #[describe("Tip cap in gwei")]
    #[label(name)]
    tipcap_gwei: gauge,
    #[describe("Blob fee cap in gwei")]
    #[label(name)]
    blob_fee_gwei: gauge,
    #[describe("Number of RPC errors")]
    #[label(name)]
    rpc_error_count: counter,
    #[describe("Number of confirmed transactions")]
    #[label(name)]
    tx_confirmed_count: counter,
    #[describe("Number of failed send attempts (includes timeouts where the tx may still confirm)")]
    #[label(name)]
    tx_failed_count: counter,
}

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

    /// Record the blob fee cap in gwei (fractional precision preserved).
    fn record_blob_fee(&self, blob_fee_gwei: f64);

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
    fn record_blob_fee(&self, _blob_fee_gwei: f64) {}
    fn record_rpc_error(&self) {}
    fn record_tx_confirmed(&self) {}
    fn record_tx_failed(&self) {}
}

/// Production [`TxMetrics`] implementation backed by the [`metrics`] crate.
///
/// Each method delegates to the generated [`TxManagerMetrics`] static accessors.
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
    ///
    /// All counters and gauges are zero-initialized so they appear
    /// immediately in the metrics endpoint.
    pub fn new(name: &'static str) -> Self {
        let this = Self { name };
        TxManagerMetrics::tx_gas_bump_count(name).absolute(0);
        TxManagerMetrics::current_nonce(name).set(0.0);
        TxManagerMetrics::tx_publish_error_count(name).absolute(0);
        TxManagerMetrics::basefee_gwei(name).set(0.0);
        TxManagerMetrics::tipcap_gwei(name).set(0.0);
        TxManagerMetrics::blob_fee_gwei(name).set(0.0);
        TxManagerMetrics::rpc_error_count(name).absolute(0);
        TxManagerMetrics::tx_confirmed_count(name).absolute(0);
        TxManagerMetrics::tx_failed_count(name).absolute(0);
        this
    }
}

impl TxMetrics for BaseTxMetrics {
    fn record_tx_max_fee(&self, fee_gwei: f64) {
        TxManagerMetrics::tx_max_fee_gwei(self.name).record(fee_gwei);
    }

    fn record_gas_bump(&self) {
        TxManagerMetrics::tx_gas_bump_count(self.name).increment(1);
    }

    fn record_send_latency(&self, latency_ms: u64) {
        TxManagerMetrics::tx_send_latency_ms(self.name).record(latency_ms as f64);
    }

    fn record_current_nonce(&self, nonce: u64) {
        TxManagerMetrics::current_nonce(self.name).set(nonce as f64);
    }

    fn record_publish_error(&self) {
        TxManagerMetrics::tx_publish_error_count(self.name).increment(1);
    }

    fn record_basefee(&self, basefee_gwei: f64) {
        TxManagerMetrics::basefee_gwei(self.name).set(basefee_gwei);
    }

    fn record_tipcap(&self, tipcap_gwei: f64) {
        TxManagerMetrics::tipcap_gwei(self.name).set(tipcap_gwei);
    }

    fn record_blob_fee(&self, blob_fee_gwei: f64) {
        TxManagerMetrics::blob_fee_gwei(self.name).set(blob_fee_gwei);
    }

    fn record_rpc_error(&self) {
        TxManagerMetrics::rpc_error_count(self.name).increment(1);
    }

    fn record_tx_confirmed(&self) {
        TxManagerMetrics::tx_confirmed_count(self.name).increment(1);
    }

    fn record_tx_failed(&self) {
        TxManagerMetrics::tx_failed_count(self.name).increment(1);
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
        m.record_blob_fee(1.0);
        m.record_rpc_error();
        m.record_tx_confirmed();
        m.record_tx_failed();
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
        m.record_blob_fee(1.0);
        m.record_rpc_error();
        m.record_tx_confirmed();
        m.record_tx_failed();
    }
}
