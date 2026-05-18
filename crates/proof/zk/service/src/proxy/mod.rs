//! Rate-limited reverse-proxy servers for L1, L2, and Beacon RPC endpoints.

mod config;
pub use config::{ProxyConfig, ProxyConfigs, RateLimitConfig};

mod rate_limit;

mod server;

use server::start_proxy;
use tokio::task::JoinHandle;
use tracing::{error, info};

/// Start all proxy servers (L1, L2, Beacon) as background tasks.
/// Returns handles to the spawned tasks.
pub async fn start_all_proxies(configs: ProxyConfigs) -> anyhow::Result<Vec<JoinHandle<()>>> {
    configs.validate()?;

    info!("Starting RPC proxy servers...");
    info!(
        l1_local = %configs.l1.local_address(),
        l1_backend = %configs.l1.backend_url,
        "L1 proxy"
    );
    info!(
        l2_local = %configs.l2.local_address(),
        l2_backend = %configs.l2.backend_url,
        "L2 proxy"
    );
    info!(
        beacon_local = %configs.beacon.local_address(),
        beacon_backend = %configs.beacon.backend_url,
        "Beacon proxy"
    );
    info!(
        requests_per_second = configs.l1.requests_per_second,
        max_concurrent_requests = configs.l1.max_concurrent_requests,
        queue_timeout_secs = configs.l1.queue_timeout.as_secs(),
        "Rate limits"
    );

    let mut handles = Vec::new();

    // Spawn L1 proxy
    let l1_config = configs.l1.clone();
    let l1_handle = tokio::spawn(async move {
        if let Err(err) = start_proxy(l1_config).await {
            error!(error = %err, "L1 proxy server failed");
        }
    });
    handles.push(l1_handle);

    // Spawn L2 proxy
    let l2_config = configs.l2.clone();
    let l2_handle = tokio::spawn(async move {
        if let Err(err) = start_proxy(l2_config).await {
            error!(error = %err, "L2 proxy server failed");
        }
    });
    handles.push(l2_handle);

    // Spawn Beacon proxy
    let beacon_config = configs.beacon;
    let beacon_handle = tokio::spawn(async move {
        if let Err(err) = start_proxy(beacon_config).await {
            error!(error = %err, "Beacon proxy server failed");
        }
    });
    handles.push(beacon_handle);

    // Give servers a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    info!("All proxy servers started successfully");

    Ok(handles)
}

#[cfg(test)]
mod tests {
    use super::config::{ProxyConfig, RateLimitConfig};

    fn default_rate_limit() -> RateLimitConfig {
        RateLimitConfig {
            requests_per_second: 100,
            max_concurrent_requests: 10,
            queue_timeout_secs: 5,
        }
    }

    #[test]
    fn test_proxy_config_basic() {
        let rl = default_rate_limit();
        let config = ProxyConfig::new(18545, "http://example.com".to_string(), &rl);
        assert!(config.validate().is_ok());
        assert_eq!(config.local_address(), "http://localhost:18545");
    }

    #[test]
    fn test_config_validation_zero_rps() {
        let rl = RateLimitConfig {
            requests_per_second: 0,
            max_concurrent_requests: 10,
            queue_timeout_secs: 5,
        };
        let config = ProxyConfig::new(18546, "http://example.com".to_string(), &rl);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_zero_concurrency() {
        let rl = RateLimitConfig {
            requests_per_second: 100,
            max_concurrent_requests: 0,
            queue_timeout_secs: 5,
        };
        let config = ProxyConfig::new(18546, "http://example.com".to_string(), &rl);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_empty_backend() {
        let rl = default_rate_limit();
        let config = ProxyConfig::new(18546, String::new(), &rl);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_validation_valid() {
        let rl = default_rate_limit();
        let config = ProxyConfig::new(18546, "http://example.com".to_string(), &rl);
        assert!(config.validate().is_ok());
    }
}
