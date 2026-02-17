//! Contains the [`BackendConfig`] struct, which is used to configure backends in Roxy.

use core::time::Duration;

/// Configuration for HTTP backend.
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Request timeout.
    pub timeout: Duration,
    /// Maximum retry attempts.
    pub max_retries: u32,
    /// Maximum batch size.
    pub max_batch_size: usize,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(30), max_retries: 3, max_batch_size: 100 }
    }
}
