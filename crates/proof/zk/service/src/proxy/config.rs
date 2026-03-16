use std::time::Duration;

/// Shared rate-limiting parameters applied to all proxy instances.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Maximum requests per second.
    pub requests_per_second: u32,
    /// Maximum concurrent in-flight requests.
    pub max_concurrent_requests: usize,
    /// Timeout (in seconds) for requests waiting in the queue.
    pub queue_timeout_secs: u64,
}

/// Configuration for a single proxy instance.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Local port to bind the proxy server.
    pub local_port: u16,
    /// Backend URL to forward requests to.
    pub backend_url: String,
    /// Rate limit: requests per second.
    pub requests_per_second: u32,
    /// Maximum concurrent in-flight requests.
    pub max_concurrent_requests: usize,
    /// Timeout for queued requests.
    pub queue_timeout: Duration,
}

impl ProxyConfig {
    /// Creates a proxy configuration from endpoint details and shared rate-limit settings.
    pub const fn new(local_port: u16, backend_url: String, rate_limit: &RateLimitConfig) -> Self {
        Self {
            local_port,
            backend_url,
            requests_per_second: rate_limit.requests_per_second,
            max_concurrent_requests: rate_limit.max_concurrent_requests,
            queue_timeout: Duration::from_secs(rate_limit.queue_timeout_secs),
        }
    }

    /// Validates rate-limit and endpoint constraints for this proxy.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.requests_per_second == 0 {
            anyhow::bail!("requests_per_second must be > 0");
        }
        if self.max_concurrent_requests == 0 {
            anyhow::bail!("max_concurrent_requests must be > 0");
        }
        if self.backend_url.is_empty() {
            anyhow::bail!("backend_url cannot be empty");
        }
        Ok(())
    }

    /// Returns the local HTTP listen address (`address=http://localhost:<port>`).
    pub fn local_address(&self) -> String {
        format!("http://localhost:{}", self.local_port)
    }
}

/// Configuration for all proxy instances.
#[derive(Debug, Clone)]
pub struct ProxyConfigs {
    /// Proxy config for L1 RPC traffic.
    pub l1: ProxyConfig,
    /// Proxy config for L2 RPC traffic.
    pub l2: ProxyConfig,
    /// Proxy config for beacon API traffic.
    pub beacon: ProxyConfig,
}

impl ProxyConfigs {
    /// Creates a bundled proxy config for L1, L2, and beacon endpoints.
    pub const fn new(
        l1_port: u16,
        l1_backend: String,
        l2_port: u16,
        l2_backend: String,
        beacon_port: u16,
        beacon_backend: String,
        rate_limit: RateLimitConfig,
    ) -> Self {
        Self {
            l1: ProxyConfig::new(l1_port, l1_backend, &rate_limit),
            l2: ProxyConfig::new(l2_port, l2_backend, &rate_limit),
            beacon: ProxyConfig::new(beacon_port, beacon_backend, &rate_limit),
        }
    }

    /// Validates all proxy endpoint configurations.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.l1.validate()?;
        self.l2.validate()?;
        self.beacon.validate()?;
        Ok(())
    }
}
