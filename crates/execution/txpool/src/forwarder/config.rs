use std::time::Duration;

use base_bundles::SharedInlineMetering;
use url::Url;

/// Configuration for the transaction forwarder.
///
/// One forwarder is spawned per builder URL. Each subscribes to the consumer's
/// broadcast channel and forwards transactions via `base_insertValidatedTransaction`.
/// Under normal load, transactions are sent immediately (batch of 1). When the
/// sliding window rate limit is hit, incoming transactions buffer and flush as
/// a single batch once the window opens.
#[derive(Debug, Clone)]
pub struct ForwarderConfig {
    /// Builder RPC endpoint URLs — one forwarder task per URL.
    pub builder_urls: Vec<Url>,
    /// Maximum RPC requests per second per forwarder (sliding window). 0 = unlimited.
    pub max_rps: u32,
    /// Maximum transactions per RPC request. 0 = unlimited.
    pub max_batch_size: usize,
    /// Maximum RPC send retries before dropping a batch.
    pub max_retries: u32,
    /// Base delay between retries (doubles each attempt).
    pub retry_backoff: Duration,
    /// Per-request timeout for the HTTP client.
    pub request_timeout: Duration,
    /// When set with [`Self::require_metering`], gates forwarding on inline meterBundle results.
    pub inline_metering: Option<SharedInlineMetering>,
    /// When true, transactions are not forwarded until a meterBundle response exists.
    pub require_metering: bool,
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            builder_urls: Vec::new(),
            max_rps: 200,
            max_batch_size: 500,
            max_retries: 3,
            retry_backoff: Duration::from_millis(100),
            request_timeout: Duration::from_secs(1),
            inline_metering: None,
            require_metering: false,
        }
    }
}

impl ForwarderConfig {
    /// Sets the builder URLs.
    pub fn with_builder_urls(mut self, urls: Vec<Url>) -> Self {
        self.builder_urls = urls;
        self
    }

    /// Sets the maximum RPC requests per second.
    pub const fn with_max_rps(mut self, rps: u32) -> Self {
        self.max_rps = rps;
        self
    }

    /// Sets the maximum batch size per request.
    pub const fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = size;
        self
    }

    /// Sets the max retries.
    pub const fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Sets the retry backoff.
    pub const fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Sets the per-request HTTP timeout.
    pub const fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Sets the shared inline metering handle.
    pub fn with_inline_metering(mut self, metering: SharedInlineMetering) -> Self {
        self.inline_metering = Some(metering);
        self
    }

    /// Requires a meterBundle response before forwarding.
    pub const fn with_require_metering(mut self, require: bool) -> Self {
        self.require_metering = require;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = ForwarderConfig::default();
        assert!(config.builder_urls.is_empty());
        assert_eq!(config.max_rps, 200);
        assert_eq!(config.max_batch_size, 500);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_backoff, Duration::from_millis(100));
        assert_eq!(config.request_timeout, Duration::from_secs(1));
        assert!(!config.require_metering);
        assert!(config.inline_metering.is_none());
    }

    #[test]
    fn builder_methods() {
        let url: Url = "http://builder1:8545".parse().unwrap();
        let config = ForwarderConfig::default()
            .with_builder_urls(vec![url.clone()])
            .with_max_rps(500)
            .with_max_batch_size(200)
            .with_max_retries(5)
            .with_retry_backoff(Duration::from_millis(250))
            .with_request_timeout(Duration::from_millis(500))
            .with_require_metering(true);

        assert_eq!(config.builder_urls, vec![url]);
        assert_eq!(config.max_rps, 500);
        assert_eq!(config.max_batch_size, 200);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.retry_backoff, Duration::from_millis(250));
        assert_eq!(config.request_timeout, Duration::from_millis(500));
        assert!(config.require_metering);
    }

    #[test]
    fn zero_means_unlimited() {
        let config = ForwarderConfig::default().with_max_rps(0).with_max_batch_size(0);
        assert_eq!(config.max_rps, 0);
        assert_eq!(config.max_batch_size, 0);
    }
}
