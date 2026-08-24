use std::time::Duration;

use crate::config::{DEFAULT_MAX_BATCH_SIZE, DEFAULT_MAX_RPS};

/// Configuration for transaction forwarders.
///
/// Each forwarder receives from its own queue and forwards transactions via
/// `base_insertValidatedTransaction`.
/// An isolated request is sent immediately, but any other requests already
/// waiting in the queue are drained into the same batch (capped at
/// `max_batch_size`). When the sliding window rate limit is hit, incoming
/// transactions buffer and flush as a single batch once the window opens.
#[derive(Debug, Clone)]
pub(crate) struct ForwarderConfig {
    /// Maximum RPC requests per second per forwarder (sliding window). 0 = unlimited.
    pub(crate) max_rps: u32,
    /// Maximum transactions per RPC request. 0 = unlimited.
    pub(crate) max_batch_size: usize,
    /// Maximum RPC send retries before dropping a batch.
    pub(crate) max_retries: u32,
    /// Base delay between retries (doubles each attempt).
    pub(crate) retry_backoff: Duration,
    /// Per-request timeout for the HTTP client.
    pub(crate) request_timeout: Duration,
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            max_rps: DEFAULT_MAX_RPS,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            max_retries: 3,
            retry_backoff: Duration::from_millis(100),
            request_timeout: Duration::from_secs(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = ForwarderConfig::default();
        assert_eq!(config.max_rps, DEFAULT_MAX_RPS);
        assert_eq!(config.max_batch_size, DEFAULT_MAX_BATCH_SIZE);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_backoff, Duration::from_millis(100));
        assert_eq!(config.request_timeout, Duration::from_secs(1));
    }

    #[test]
    fn zero_means_unlimited() {
        let config = ForwarderConfig { max_rps: 0, max_batch_size: 0, ..Default::default() };
        assert_eq!(config.max_rps, 0);
        assert_eq!(config.max_batch_size, 0);
    }
}
