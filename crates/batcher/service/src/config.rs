//! Full batcher runtime configuration.

use std::{fmt, net::SocketAddr, str::FromStr, time::Duration};

use alloy_primitives::B256;
use base_batcher_core::ThrottleConfig;
use base_batcher_encoder::EncoderConfig;
use url::Url;

/// A batcher private key that redacts itself in [`fmt::Debug`] output to
/// prevent accidental logging.
#[derive(Clone)]
pub struct SecretKey(pub B256);

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl FromStr for SecretKey {
    type Err = <B256 as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<B256>().map(SecretKey)
    }
}

/// Full batcher configuration combining RPC endpoints, identity, encoding
/// parameters, submission limits, and optional throttling.
///
/// The batch inbox address is sourced from the rollup config fetched at startup
/// via `optimism_rollupConfig`, so it is not stored here.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// L1 RPC endpoint.
    pub l1_rpc_url: Url,
    /// L2 HTTP RPC endpoint. Used for all JSON-RPC calls including throttle
    /// control (`miner_setMaxDASize`). Must be an HTTP or HTTPS URL.
    pub l2_rpc_url: Url,
    /// Optional L1 WebSocket endpoint for new-block subscriptions.
    ///
    /// When set, the batcher subscribes to new L1 block headers over this
    /// connection to advance the pipeline's L1 head, falling back to polling
    /// [`l1_rpc_url`] only on failure. When absent, polling is used exclusively.
    pub l1_ws_url: Option<Url>,
    /// Optional L2 WebSocket endpoint for new-block subscriptions.
    ///
    /// When set, the batcher subscribes to new block headers over this
    /// connection and falls back to polling [`l2_rpc_url`] only on failure.
    /// When absent, the batcher uses polling exclusively.
    pub l2_ws_url: Option<Url>,
    /// Rollup node RPC endpoint.
    pub rollup_rpc_url: Url,
    /// Private key for signing L1 transactions.
    pub batcher_private_key: SecretKey,
    /// L2 block polling interval.
    pub poll_interval: Duration,
    /// Encoder configuration.
    pub encoder_config: EncoderConfig,
    /// Maximum number of in-flight (unconfirmed) transactions.
    pub max_pending_transactions: usize,
    /// Number of L1 confirmations before a tx is considered finalized.
    pub num_confirmations: usize,
    /// Timeout before resubmitting a transaction.
    pub resubmission_timeout: Duration,
    /// Throttle configuration (optional).
    pub throttle: Option<ThrottleConfig>,
    /// Socket address for the admin JSON-RPC API.
    ///
    /// When set, the batcher exposes the `admin_*` RPC namespace on this address.
    /// When `None` (the default), the admin server is disabled.
    pub admin_addr: Option<SocketAddr>,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            l1_rpc_url: "http://localhost:8545".parse().expect("valid default URL"),
            l1_ws_url: None,
            l2_rpc_url: "http://localhost:9545".parse().expect("valid default URL"),
            l2_ws_url: None,
            rollup_rpc_url: "http://localhost:7545".parse().expect("valid default URL"),
            batcher_private_key: SecretKey(B256::ZERO),
            poll_interval: Duration::from_secs(1),
            encoder_config: EncoderConfig::default(),
            max_pending_transactions: 1,
            num_confirmations: 1,
            resubmission_timeout: Duration::from_secs(48),
            throttle: Some(ThrottleConfig::default()),
            admin_addr: None,
        }
    }
}
