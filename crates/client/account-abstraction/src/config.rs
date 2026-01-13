//! Account Abstraction configuration
//!
//! This module provides configuration management for EIP-4337 account abstraction.
//! Configuration is provided via CLI flags.
//!
//! # Send Modes
//!
//! The AA RPC supports two mutually exclusive modes for handling `eth_sendUserOperation`:
//!
//! - **Tips Mode** (default): UserOps are forwarded to an external TIPS service
//! - **Mempool Mode**: UserOps are stored in a local mempool (for sequencer nodes)
//!
//! When mempool mode is enabled, p2p gossip can optionally be configured to share
//! UserOps with other sequencer nodes in a private network.

use thiserror::Error;
use url::Url;

use crate::mempool::{GossipConfig, MempoolConfig};

/// Errors that can occur when validating config
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Config validation failed: {0}")]
    ValidationError(String),

    #[error("Invalid URL '{url}': {reason}")]
    InvalidUrl { url: String, reason: String },
}

/// Default number of blocks to look back when searching for UserOperationEvent logs
const DEFAULT_USER_OP_EVENT_LOOKBACK_BLOCKS: u64 = 1000;

/// Default mempool configuration values
const DEFAULT_MAX_OPS_PER_SENDER: usize = 4;
const DEFAULT_MAX_POOL_SIZE_PER_ENTRYPOINT: usize = 10_000;
const DEFAULT_P2P_PORT: u16 = 9545;
const DEFAULT_MAX_PEERS: u32 = 50;

/// The mode for handling eth_sendUserOperation
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SendMode {
    /// Forward UserOps to an external TIPS service (default)
    #[default]
    Tips,
    /// Store UserOps in a local mempool (for sequencer nodes)
    Mempool,
}

/// Account Abstraction CLI arguments
///
/// # Example usage:
///
/// ## Tips Mode (default - relay to external service):
/// ```bash
/// base-reth-node \
///     --account-abstraction.enabled \
///     --account-abstraction.send-url http://tips-service:8080
/// ```
///
/// ## Mempool Mode (local storage for sequencer nodes):
/// ```bash
/// base-reth-node \
///     --account-abstraction.enabled \
///     --account-abstraction.mempool-enabled \
///     --account-abstraction.p2p-enabled \
///     --account-abstraction.p2p-port 9545 \
///     --account-abstraction.p2p-peers /ip4/10.0.0.1/tcp/9545/p2p/12D3...
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Account Abstraction (ERC-4337)")]
pub struct AccountAbstractionArgs {
    /// Enable Account Abstraction (ERC-4337) RPC endpoints
    #[arg(long = "account-abstraction.enabled", default_value = "false")]
    pub enabled: bool,

    /// URL for submitting user operations (TIPS ingress service).
    /// Required when mempool mode is disabled.
    #[arg(long = "account-abstraction.send-url")]
    pub send_url: Option<String>,

    /// Enable debug logging for account abstraction operations
    #[arg(long = "account-abstraction.debug", default_value = "false")]
    pub debug: bool,

    /// Number of blocks to look back when searching for UserOperationEvent logs.
    /// Used as fallback when the AA indexer ExEx is not installed.
    #[arg(
        long = "account-abstraction.event-lookback-blocks",
        default_value_t = DEFAULT_USER_OP_EVENT_LOOKBACK_BLOCKS
    )]
    pub user_op_event_lookback_blocks: u64,

    /// Enable Account Abstraction (ERC-4337) UserOperation indexer ExEx
    #[arg(long = "account-abstraction.indexer", default_value = "false")]
    pub indexer_enabled: bool,

    // ========== Mempool Mode Options ==========
    /// Enable local UserOperation mempool mode.
    /// When enabled, UserOps are stored locally instead of forwarded to TIPS.
    /// This is typically used by sequencer nodes.
    #[arg(long = "account-abstraction.mempool-enabled", default_value = "false")]
    pub mempool_enabled: bool,

    /// Maximum number of UserOps per sender in the mempool.
    #[arg(
        long = "account-abstraction.max-ops-per-sender",
        default_value_t = DEFAULT_MAX_OPS_PER_SENDER
    )]
    pub max_ops_per_sender: usize,

    /// Maximum total UserOps per entrypoint in the mempool.
    #[arg(
        long = "account-abstraction.max-pool-size",
        default_value_t = DEFAULT_MAX_POOL_SIZE_PER_ENTRYPOINT
    )]
    pub max_pool_size_per_entrypoint: usize,

    // ========== P2P Options (requires mempool mode) ==========
    /// Enable p2p gossip for UserOperation sharing.
    /// Requires mempool mode. Used for private sequencer networks.
    #[arg(long = "account-abstraction.p2p-enabled", default_value = "false")]
    pub p2p_enabled: bool,

    /// Port for p2p UserOperation gossip.
    #[arg(
        long = "account-abstraction.p2p-port",
        default_value_t = DEFAULT_P2P_PORT
    )]
    pub p2p_port: u16,

    /// Known peers for p2p gossip (multiaddr format).
    /// Can be specified multiple times.
    /// Example: /ip4/10.0.0.1/tcp/9545/p2p/12D3KooW...
    #[arg(long = "account-abstraction.p2p-peers")]
    pub p2p_peers: Vec<String>,

    /// Hex-encoded ed25519 private key for p2p node identity.
    /// If not provided, a random key is generated on each startup.
    #[arg(long = "account-abstraction.p2p-keypair")]
    pub p2p_keypair: Option<String>,

    /// Maximum number of p2p peers to connect to.
    #[arg(
        long = "account-abstraction.p2p-max-peers",
        default_value_t = DEFAULT_MAX_PEERS
    )]
    pub p2p_max_peers: u32,
}

impl AccountAbstractionArgs {
    /// Validate the configuration
    ///
    /// Checks that all required fields are present and valid when AA is enabled.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }

        // Determine the send mode
        let mode = self.send_mode();

        match mode {
            SendMode::Tips => {
                // In Tips mode, send_url is required
                let send_url = self.send_url.as_ref().ok_or_else(|| {
                    ConfigError::ValidationError(
                        "--account-abstraction.send-url is required when mempool mode is disabled"
                            .to_string(),
                    )
                })?;

                // Validate send_url is not empty
                if send_url.trim().is_empty() {
                    return Err(ConfigError::ValidationError(
                        "--account-abstraction.send-url cannot be empty".to_string(),
                    ));
                }

                // Validate send_url is a valid URL
                Url::parse(send_url).map_err(|e| ConfigError::InvalidUrl {
                    url: send_url.clone(),
                    reason: e.to_string(),
                })?;
            }
            SendMode::Mempool => {
                // P2P requires mempool mode (which is already true here)
                // Validate p2p peers are valid multiaddrs if provided
                for peer in &self.p2p_peers {
                    peer.parse::<p2p::Multiaddr>().map_err(|e| {
                        ConfigError::ValidationError(format!(
                            "Invalid p2p peer multiaddr '{}': {}",
                            peer, e
                        ))
                    })?;
                }
            }
        }

        Ok(())
    }

    /// Get the send mode (Tips or Mempool)
    pub fn send_mode(&self) -> SendMode {
        if self.mempool_enabled {
            SendMode::Mempool
        } else {
            SendMode::Tips
        }
    }

    /// Get the send URL as a parsed URL
    ///
    /// # Panics
    /// Panics if the URL is invalid or not set. Only valid in Tips mode.
    pub fn send_url(&self) -> Url {
        Url::parse(
            self.send_url
                .as_ref()
                .expect("send_url should be validated before use"),
        )
        .expect("send_url should be validated before use")
    }

    /// Get the number of blocks to look back when searching for UserOperationEvent logs
    pub fn user_op_event_lookback_blocks(&self) -> u64 {
        self.user_op_event_lookback_blocks
    }

    /// Check if debug mode is enabled
    pub fn is_debug(&self) -> bool {
        self.debug
    }

    /// Build a MempoolConfig from the CLI args
    pub fn mempool_config(&self) -> MempoolConfig {
        MempoolConfig::default()
            .with_max_ops_per_sender(self.max_ops_per_sender)
            .with_max_pool_size(self.max_pool_size_per_entrypoint)
    }

    /// Build a GossipConfig from the CLI args (if p2p is enabled)
    pub fn gossip_config(&self) -> Option<GossipConfig> {
        if !self.p2p_enabled {
            return None;
        }

        let peers: Vec<p2p::Multiaddr> = self
            .p2p_peers
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        let mut config = GossipConfig::default()
            .with_port(self.p2p_port)
            .with_known_peers(peers)
            .with_max_peers(self.p2p_max_peers);

        if let Some(ref keypair) = self.p2p_keypair {
            config = config.with_keypair_hex(keypair.clone());
        }

        Some(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> AccountAbstractionArgs {
        AccountAbstractionArgs {
            enabled: false,
            send_url: None,
            debug: false,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
            mempool_enabled: false,
            max_ops_per_sender: DEFAULT_MAX_OPS_PER_SENDER,
            max_pool_size_per_entrypoint: DEFAULT_MAX_POOL_SIZE_PER_ENTRYPOINT,
            p2p_enabled: false,
            p2p_port: DEFAULT_P2P_PORT,
            p2p_peers: vec![],
            p2p_keypair: None,
            p2p_max_peers: DEFAULT_MAX_PEERS,
        }
    }

    #[test]
    fn test_disabled_config_validates() {
        let args = base_args();
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_tips_mode_requires_send_url() {
        let mut args = base_args();
        args.enabled = true;
        // Tips mode (default) requires send_url
        assert!(matches!(
            args.validate(),
            Err(ConfigError::ValidationError(_))
        ));
    }

    #[test]
    fn test_tips_mode_with_valid_url() {
        let mut args = base_args();
        args.enabled = true;
        args.send_url = Some("http://localhost:8080".to_string());
        args.debug = true;

        assert!(args.validate().is_ok());
        assert_eq!(args.send_mode(), SendMode::Tips);
        assert_eq!(args.send_url().as_str(), "http://localhost:8080/");
    }

    #[test]
    fn test_empty_send_url_fails() {
        let mut args = base_args();
        args.enabled = true;
        args.send_url = Some("".to_string());

        assert!(matches!(
            args.validate(),
            Err(ConfigError::ValidationError(_))
        ));
    }

    #[test]
    fn test_invalid_url_fails() {
        let mut args = base_args();
        args.enabled = true;
        args.send_url = Some("not-a-valid-url".to_string());

        assert!(matches!(args.validate(), Err(ConfigError::InvalidUrl { .. })));
    }

    #[test]
    fn test_mempool_mode_validates_without_send_url() {
        let mut args = base_args();
        args.enabled = true;
        args.mempool_enabled = true;
        // No send_url needed in mempool mode

        assert!(args.validate().is_ok());
        assert_eq!(args.send_mode(), SendMode::Mempool);
    }

    #[test]
    fn test_mempool_config() {
        let mut args = base_args();
        args.max_ops_per_sender = 8;
        args.max_pool_size_per_entrypoint = 5000;

        let config = args.mempool_config();
        assert_eq!(config.max_ops_per_sender, 8);
        assert_eq!(config.max_pool_size_per_entrypoint, 5000);
    }

    #[test]
    fn test_gossip_config_disabled() {
        let args = base_args();
        assert!(args.gossip_config().is_none());
    }

    #[test]
    fn test_gossip_config_enabled() {
        let mut args = base_args();
        args.p2p_enabled = true;
        args.p2p_port = 9999;
        args.p2p_max_peers = 100;

        let config = args.gossip_config().expect("should have config");
        assert_eq!(config.port, 9999);
        assert_eq!(config.max_peers, 100);
    }

    #[test]
    fn test_default_lookback_blocks() {
        let args = AccountAbstractionArgs::default();
        // Default is set via clap, so in code it will be 0 unless parsed
        // This tests the getter works
        assert_eq!(args.user_op_event_lookback_blocks(), 0);
    }

    #[test]
    fn test_default_send_mode_is_tips() {
        let args = AccountAbstractionArgs::default();
        assert_eq!(args.send_mode(), SendMode::Tips);
    }
}
