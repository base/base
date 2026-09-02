//! Configuration for the transaction forwarding extension.

use std::time::Duration;

use url::Url;

use crate::{forwarder::ForwarderConfig, reader::ReaderConfig};

/// Default resend-after window in milliseconds (~2 blocks on Base).
pub const DEFAULT_RESEND_AFTER_MS: u64 = 4000;
/// Default maximum number of transactions per RPC batch.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 100;
/// Default maximum RPC requests per second per forwarder.
pub const DEFAULT_MAX_RPS: u32 = 200;
/// Default number of in-process meter_bundle workers.
pub const DEFAULT_INLINE_SIMULATION_WORKERS: usize = 16;
/// Default capacity of the pre-sim queue.
pub const DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY: usize = 10_000;
/// Default per-transaction meter_bundle timeout in milliseconds.
pub const DEFAULT_INLINE_SIMULATION_TIMEOUT_MS: u64 = 2_000;

/// Full configuration for the transaction forwarding extension.
#[derive(Debug, Clone)]
pub struct TxForwardingConfig {
    /// Whether transaction forwarding is enabled.
    pub enabled: bool,
    /// Builder RPC endpoints to forward transactions to.
    pub builder_urls: Vec<Url>,
    /// Resend transactions that haven't been included after this duration in milliseconds.
    pub resend_after_ms: u64,
    /// Maximum number of transactions per batch (0 = unlimited).
    pub max_batch_size: usize,
    /// Maximum RPC requests per second per forwarder (0 = unlimited).
    pub max_rps: u32,
    /// When true, meter_bundle runs on the mempool node before pool insert.
    ///
    /// Stored only until the sim-worker path is wired; forwarding behavior is
    /// unchanged while this flag is unused.
    pub inline_simulation: bool,
    /// Number of meter_bundle worker tasks.
    pub inline_simulation_workers: usize,
    /// Bounded pre-sim queue capacity.
    pub inline_simulation_queue_capacity: usize,
    /// Per-transaction meter_bundle timeout in milliseconds.
    pub inline_simulation_timeout_ms: u64,
}

impl Default for TxForwardingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            builder_urls: Vec::new(),
            // Default: 2 blocks (~4 seconds on Base)
            resend_after_ms: DEFAULT_RESEND_AFTER_MS,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            max_rps: DEFAULT_MAX_RPS,
            inline_simulation: false,
            inline_simulation_workers: DEFAULT_INLINE_SIMULATION_WORKERS,
            inline_simulation_queue_capacity: DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY,
            inline_simulation_timeout_ms: DEFAULT_INLINE_SIMULATION_TIMEOUT_MS,
        }
    }
}

impl TxForwardingConfig {
    /// Creates a disabled configuration.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Creates a new configuration with forwarding enabled.
    pub fn new(builder_urls: Vec<Url>) -> Self {
        Self { enabled: true, builder_urls, ..Default::default() }
    }

    /// Sets the resend-after window in milliseconds.
    pub const fn with_resend_after_ms(mut self, ms: u64) -> Self {
        self.resend_after_ms = ms;
        self
    }

    /// Sets the maximum batch size per RPC request.
    pub const fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = size;
        self
    }

    /// Sets the maximum RPC requests per second.
    pub const fn with_max_rps(mut self, rps: u32) -> Self {
        self.max_rps = rps;
        self
    }

    /// Enables or disables in-process meter_bundle before pool insert.
    pub const fn with_inline_simulation(mut self, enabled: bool) -> Self {
        self.inline_simulation = enabled;
        self
    }

    /// Sets the number of meter_bundle worker tasks.
    pub const fn with_inline_simulation_workers(mut self, workers: usize) -> Self {
        self.inline_simulation_workers = workers;
        self
    }

    /// Sets the pre-sim queue capacity.
    pub const fn with_inline_simulation_queue_capacity(mut self, capacity: usize) -> Self {
        self.inline_simulation_queue_capacity = capacity;
        self
    }

    /// Sets the per-transaction meter_bundle timeout in milliseconds.
    pub const fn with_inline_simulation_timeout_ms(mut self, ms: u64) -> Self {
        self.inline_simulation_timeout_ms = ms;
        self
    }

    /// Builds the per-destination reader configuration.
    pub(crate) fn reader_config(&self) -> ReaderConfig {
        ReaderConfig {
            resend_after: Duration::from_millis(self.resend_after_ms),
            ..Default::default()
        }
    }

    /// Builds the destination forwarder configuration.
    pub(crate) fn forwarder_config(&self) -> ForwarderConfig {
        ForwarderConfig {
            max_batch_size: self.max_batch_size,
            max_rps: self.max_rps,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_simulation_defaults_off() {
        let config = TxForwardingConfig::default();

        assert!(!config.inline_simulation);
        assert_eq!(config.inline_simulation_workers, DEFAULT_INLINE_SIMULATION_WORKERS);
        assert_eq!(
            config.inline_simulation_queue_capacity,
            DEFAULT_INLINE_SIMULATION_QUEUE_CAPACITY
        );
        assert_eq!(config.inline_simulation_timeout_ms, DEFAULT_INLINE_SIMULATION_TIMEOUT_MS);
    }

    #[test]
    fn new_enables_forwarding_without_inline_simulation() {
        let config = TxForwardingConfig::new(vec!["http://builder.test".parse().unwrap()]);

        assert!(config.enabled);
        assert!(!config.inline_simulation);
        assert_eq!(config.inline_simulation_workers, DEFAULT_INLINE_SIMULATION_WORKERS);
    }

    #[test]
    fn inline_simulation_builders_override_defaults() {
        let config = TxForwardingConfig::new(vec!["http://builder.test".parse().unwrap()])
            .with_inline_simulation(true)
            .with_inline_simulation_workers(8)
            .with_inline_simulation_queue_capacity(32)
            .with_inline_simulation_timeout_ms(500);

        assert!(config.inline_simulation);
        assert_eq!(config.inline_simulation_workers, 8);
        assert_eq!(config.inline_simulation_queue_capacity, 32);
        assert_eq!(config.inline_simulation_timeout_ms, 500);
    }
}
