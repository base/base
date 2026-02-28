//! Configuration types and validation for the proposer.

use std::{net::IpAddr, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_signer::k256::ecdsa::SigningKey;
use alloy_signer_local::PrivateKeySigner;
use backon::ExponentialBuilder;
use base_cli_utils::{LogConfig, MetricsConfig};
use thiserror::Error;
use url::Url;

use crate::constants::{DEFAULT_RETRY_INITIAL_DELAY, DEFAULT_RETRY_MAX_DELAY, DEFAULT_RPC_MAX_RETRIES};

/// Errors that can occur during configuration validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Invalid URL format.
    #[error("invalid {field} URL: {reason}")]
    InvalidUrl {
        /// The field name that contains the invalid URL.
        field: &'static str,
        /// The reason the URL is invalid.
        reason: String,
    },
    /// A field value is out of the allowed range.
    #[error("{field} must be {constraint}, got {value}")]
    OutOfRange {
        /// The field name that is out of range.
        field: &'static str,
        /// The constraint description.
        constraint: &'static str,
        /// The actual value.
        value: String,
    },
    /// Invalid metrics configuration.
    #[error("invalid metrics config: {0}")]
    Metrics(String),
    /// Invalid RPC configuration.
    #[error("invalid RPC config: {0}")]
    Rpc(String),
    /// Invalid signing configuration.
    #[error("invalid signing config: {0}")]
    Signing(String),
}

/// Signing configuration for L1 transaction submission.
#[derive(Clone)]
pub enum SigningConfig {
    /// Local signing with an in-process private key (development).
    Local {
        /// The private key signer.
        signer: PrivateKeySigner,
    },
    /// Remote signing via a signer sidecar JSON-RPC endpoint (production).
    Remote {
        /// URL of the signer sidecar.
        endpoint: Url,
        /// Address of the signer account.
        address: Address,
    },
}

impl std::fmt::Debug for SigningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { signer } => {
                f.debug_struct("Local").field("address", &signer.address()).finish()
            }
            Self::Remote { endpoint, address } => f
                .debug_struct("Remote")
                .field("endpoint", endpoint)
                .field("address", address)
                .finish(),
        }
    }
}

/// Validated proposer configuration.
#[derive(Debug, Clone)]
pub struct ProposerConfig {
    /// Allow proposals based on non-finalized L1 data.
    pub allow_non_finalized: bool,
    /// URL of the enclave RPC endpoint.
    pub enclave_rpc: Url,
    /// URL of the L1 Ethereum RPC endpoint.
    pub l1_eth_rpc: Url,
    /// URL of the L2 Ethereum RPC endpoint.
    pub l2_eth_rpc: Url,
    /// Use reth-specific RPC calls for L2.
    pub l2_reth: bool,
    /// Address of the `AnchorStateRegistry` contract on L1.
    pub anchor_state_registry_addr: Address,
    /// Address of the `DisputeGameFactory` contract on L1.
    pub dispute_game_factory_addr: Address,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// Keccak256 hash of the TEE image PCR0.
    pub tee_image_hash: B256,
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// RPC request timeout.
    pub rpc_timeout: Duration,
    /// URL of the rollup RPC endpoint.
    pub rollup_rpc: Url,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: bool,
    /// Wait for node sync before starting.
    pub wait_node_sync: bool,
    /// Logging configuration (from base-cli-utils).
    pub log: LogConfig,
    /// Metrics server configuration.
    pub metrics: MetricsConfig,
    /// RPC server configuration.
    pub rpc: RpcServerConfig,
    /// RPC retry configuration.
    pub retry: RetryConfig,
    /// Signing configuration for L1 transaction submission.
    pub signing: SigningConfig,
}

/// Validate that a URL has a scheme and host.
pub fn validate_url(url: &Url, field: &'static str) -> Result<(), ConfigError> {
    if url.scheme().is_empty() {
        return Err(ConfigError::InvalidUrl { field, reason: "missing scheme".to_string() });
    }

    if url.host().is_none() {
        return Err(ConfigError::InvalidUrl { field, reason: "missing host".to_string() });
    }

    Ok(())
}

/// Validate and build [`SigningConfig`] from raw arguments.
///
/// Exactly one of `private_key` or (`signer_endpoint` + `signer_address`) must be provided.
pub fn build_signing_config(
    private_key: Option<&str>,
    signer_endpoint: Option<&Url>,
    signer_address: Option<&Address>,
) -> Result<SigningConfig, ConfigError> {
    match (private_key, signer_endpoint, signer_address) {
        (Some(pk), None, None) => {
            let hex_str = pk.strip_prefix("0x").unwrap_or(pk);
            let key_bytes = hex::decode(hex_str)
                .map_err(|e| ConfigError::Signing(format!("invalid private key hex: {e}")))?;
            let signing_key = SigningKey::from_slice(&key_bytes)
                .map_err(|e| ConfigError::Signing(format!("invalid private key: {e}")))?;
            let signer = PrivateKeySigner::from_signing_key(signing_key);
            Ok(SigningConfig::Local { signer })
        }
        (None, Some(endpoint), Some(address)) => {
            validate_url(endpoint, "signer-endpoint")?;
            Ok(SigningConfig::Remote { endpoint: endpoint.clone(), address: *address })
        }
        (None, None, None) => Err(ConfigError::Signing(
            "one of --private-key or (--signer-endpoint + --signer-address) must be provided"
                .to_string(),
        )),
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(ConfigError::Signing(
            "--private-key is mutually exclusive with --signer-endpoint/--signer-address"
                .to_string(),
        )),
        (None, Some(_), None) => {
            Err(ConfigError::Signing("--signer-endpoint requires --signer-address".to_string()))
        }
        (None, None, Some(_)) => {
            Err(ConfigError::Signing("--signer-address requires --signer-endpoint".to_string()))
        }
    }
}

/// Validated RPC server configuration.
#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    /// Whether admin RPC methods are enabled.
    pub enable_admin: bool,
    /// RPC server bind address.
    pub addr: IpAddr,
    /// RPC server port.
    pub port: u16,
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self { enable_admin: false, addr: "127.0.0.1".parse().unwrap(), port: 8545 }
    }
}

/// Validated RPC retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts.
    pub max_attempts: u32,
    /// Initial delay for exponential backoff.
    pub initial_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RPC_MAX_RETRIES,
            initial_delay: DEFAULT_RETRY_INITIAL_DELAY,
            max_delay: DEFAULT_RETRY_MAX_DELAY,
        }
    }
}

impl RetryConfig {
    /// Creates a `backon` [`ExponentialBuilder`] from this configuration.
    pub fn to_backoff_builder(&self) -> ExponentialBuilder {
        ExponentialBuilder::default()
            .with_min_delay(self.initial_delay)
            .with_max_delay(self.max_delay)
            .with_max_times(self.max_attempts as usize)
            .with_jitter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_without_host() {
        let url = Url::parse("file:///some/path").unwrap();
        let result = validate_url(&url, "test-field");
        assert!(matches!(result, Err(ConfigError::InvalidUrl { field: "test-field", .. })));
    }

    #[test]
    fn test_config_error_display() {
        let error =
            ConfigError::InvalidUrl { field: "enclave-rpc", reason: "missing host".to_string() };
        assert_eq!(error.to_string(), "invalid enclave-rpc URL: missing host");

        let error = ConfigError::OutOfRange {
            field: "poll-interval",
            constraint: "greater than 0",
            value: "0".to_string(),
        };
        assert_eq!(error.to_string(), "poll-interval must be greater than 0, got 0");

        let error = ConfigError::Metrics("port must be non-zero".to_string());
        assert_eq!(error.to_string(), "invalid metrics config: port must be non-zero");

        let error = ConfigError::Rpc("RPC port must be non-zero".to_string());
        assert_eq!(error.to_string(), "invalid RPC config: RPC port must be non-zero");

        let error = ConfigError::Signing("missing key".to_string());
        assert_eq!(error.to_string(), "invalid signing config: missing key");
    }

    #[test]
    fn test_signing_config_local() {
        let result = build_signing_config(
            Some("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
            None,
            None,
        );
        assert!(matches!(result, Ok(SigningConfig::Local { .. })));
    }

    #[test]
    fn test_signing_config_remote() {
        let endpoint = Url::parse("http://localhost:8546").unwrap();
        let address: Address = "0x1234567890123456789012345678901234567890".parse().unwrap();
        let result = build_signing_config(None, Some(&endpoint), Some(&address));
        assert!(matches!(result, Ok(SigningConfig::Remote { .. })));
    }

    #[test]
    fn test_signing_config_none_provided() {
        let result = build_signing_config(None, None, None);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_signing_config_both_provided() {
        let endpoint = Url::parse("http://localhost:8546").unwrap();
        let result = build_signing_config(
            Some("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
            Some(&endpoint),
            None,
        );
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_signing_config_endpoint_without_address() {
        let endpoint = Url::parse("http://localhost:8546").unwrap();
        let result = build_signing_config(None, Some(&endpoint), None);
        assert!(matches!(result, Err(ConfigError::Signing(_))));
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_delay, Duration::from_millis(100));
        assert_eq!(config.max_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_retry_config_to_backoff_builder() {
        let config = RetryConfig {
            max_attempts: 10,
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(5),
        };
        let _ = config.to_backoff_builder();
    }
}
