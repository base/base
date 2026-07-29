//! Configuration for the transaction forwarding extension.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use base_bundles::SharedInlineMetering;
use base_execution_txpool::{
    ConsumerConfig as TxpoolConsumerConfig, ForwarderConfig as TxpoolForwarderConfig,
};
use url::Url;

/// Default resend-after window in milliseconds (~2 blocks on Base).
pub const DEFAULT_RESEND_AFTER_MS: u64 = 4000;
/// Default maximum number of transactions per RPC batch.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 100;
/// Default maximum RPC requests per second per forwarder.
pub const DEFAULT_MAX_RPS: u32 = 200;

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
    /// Require Ready meterBundle before forwarding when inline metering is available.
    pub require_metering: bool,
    /// Slot populated by [`MeteringExtension`] with the shared inline metering handle.
    pub inline_metering_slot: Option<Arc<OnceLock<SharedInlineMetering>>>,
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
            require_metering: false,
            inline_metering_slot: None,
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

    /// Requires Ready meterBundle responses before forwarding.
    pub const fn with_require_metering(mut self, require: bool) -> Self {
        self.require_metering = require;
        self
    }

    /// Sets the shared slot for the inline metering handle.
    pub fn with_inline_metering_slot(mut self, slot: Arc<OnceLock<SharedInlineMetering>>) -> Self {
        self.inline_metering_slot = Some(slot);
        self
    }

    /// Converts to the consumer config used by `base-txpool`.
    pub fn to_consumer_config(&self) -> TxpoolConsumerConfig {
        TxpoolConsumerConfig::default()
            .with_resend_after(Duration::from_millis(self.resend_after_ms))
    }

    /// Converts to the forwarder config used by `base-txpool`.
    pub fn to_forwarder_config(&self) -> TxpoolForwarderConfig {
        let mut config = TxpoolForwarderConfig::default()
            .with_builder_urls(self.builder_urls.clone())
            .with_max_batch_size(self.max_batch_size)
            .with_max_rps(self.max_rps)
            .with_require_metering(self.require_metering);
        if let Some(slot) = self.inline_metering_slot.as_ref()
            && let Some(metering) = slot.get()
        {
            config = config.with_inline_metering(Arc::clone(metering));
        }
        config
    }
}
