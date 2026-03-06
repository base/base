//! Transaction forwarding configuration types.

use std::time::Duration;

use base_txpool::{
    ConsumerConfig as TxpoolConsumerConfig, ForwarderConfig as TxpoolForwarderConfig,
};
use url::Url;

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
}

impl Default for TxForwardingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            builder_urls: Vec::new(),
            // Default: 2 blocks (~4 seconds on Base)
            resend_after_ms: 4000,
            max_batch_size: 500,
            max_rps: 200,
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

    /// Converts to the consumer config used by `base-txpool`.
    pub fn to_consumer_config(&self) -> TxpoolConsumerConfig {
        TxpoolConsumerConfig::default()
            .with_resend_after(Duration::from_millis(self.resend_after_ms))
    }

    /// Converts to the forwarder config used by `base-txpool`.
    pub fn to_forwarder_config(&self) -> TxpoolForwarderConfig {
        TxpoolForwarderConfig::default()
            .with_builder_urls(self.builder_urls.clone())
            .with_max_batch_size(self.max_batch_size)
            .with_max_rps(self.max_rps)
    }
}
