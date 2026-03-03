use std::time::Duration;

/// Configuration for the transaction forwarder.
///
/// One forwarder is spawned per builder URL. Each subscribes to the consumer's
/// broadcast channel and forwards transactions via `base_insertValidatedTransactions`.
/// Under normal load, transactions are sent immediately (batch of 1). When the
/// sliding window rate limit is hit, incoming transactions buffer and flush as
/// a single batch once the window opens.
#[derive(Debug, Clone)]
pub struct ForwarderConfig {
    /// Builder RPC endpoint URLs — one forwarder task per URL.
    pub builder_urls: Vec<String>,
    /// Maximum RPC requests per second per forwarder (sliding window). 0 = unlimited.
    pub max_rps: u32,
    /// Maximum transactions per RPC request. 0 = unlimited.
    pub max_batch_size: usize,
    /// Maximum RPC send retries before dropping a batch.
    pub max_retries: u32,
    /// Base delay between retries (doubles each attempt).
    pub retry_backoff: Duration,
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            builder_urls: Vec::new(),
            max_rps: 200,
            max_batch_size: 500,
            max_retries: 3,
            retry_backoff: Duration::from_millis(100),
        }
    }
}

impl ForwarderConfig {
    /// Sets the builder URLs.
    pub fn with_builder_urls(mut self, urls: Vec<String>) -> Self {
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
    }

    #[test]
    fn builder_methods() {
        let config = ForwarderConfig::default()
            .with_builder_urls(vec!["http://builder1:8545".into()])
            .with_max_rps(500)
            .with_max_batch_size(200)
            .with_max_retries(5)
            .with_retry_backoff(Duration::from_millis(250));

        assert_eq!(config.builder_urls, vec!["http://builder1:8545"]);
        assert_eq!(config.max_rps, 500);
        assert_eq!(config.max_batch_size, 200);
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.retry_backoff, Duration::from_millis(250));
    }

    #[test]
    fn zero_means_unlimited() {
        let config = ForwarderConfig::default().with_max_rps(0).with_max_batch_size(0);
        assert_eq!(config.max_rps, 0);
        assert_eq!(config.max_batch_size, 0);
    }
}
