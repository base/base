//! Account Abstraction configuration
//!
//! This module provides configuration management for EIP-4337 account abstraction.
//! Configuration is provided via CLI flags.

use thiserror::Error;
use url::Url;

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

/// Account Abstraction CLI arguments
///
/// # Example usage:
/// ```bash
/// base-reth-node \
///     --account-abstraction.enabled \
///     --account-abstraction.send-url http://localhost:8080 \
///     --account-abstraction.debug
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Account Abstraction (ERC-4337)")]
pub struct AccountAbstractionArgs {
    /// Enable Account Abstraction (ERC-4337) RPC endpoints
    #[arg(long = "account-abstraction.enabled", default_value = "false")]
    pub enabled: bool,

    /// URL for submitting user operations (TIPS ingress service)
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
}

impl AccountAbstractionArgs {
    /// Validate the configuration
    ///
    /// Checks that all required fields are present and valid when AA is enabled.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }

        // Validate send_url is provided when enabled
        let send_url = self.send_url.as_ref().ok_or_else(|| {
            ConfigError::ValidationError(
                "--account-abstraction.send-url is required when account abstraction is enabled"
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

        Ok(())
    }

    /// Get the send URL as a parsed URL
    ///
    /// # Panics
    /// Panics if the URL is invalid or not set. Call `validate()` first.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_config_validates() {
        let args = AccountAbstractionArgs {
            enabled: false,
            send_url: None,
            debug: false,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
        };

        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_enabled_config_requires_send_url() {
        let args = AccountAbstractionArgs {
            enabled: true,
            send_url: None,
            debug: false,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
        };

        assert!(matches!(
            args.validate(),
            Err(ConfigError::ValidationError(_))
        ));
    }

    #[test]
    fn test_enabled_config_with_valid_url() {
        let args = AccountAbstractionArgs {
            enabled: true,
            send_url: Some("http://localhost:8080".to_string()),
            debug: true,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
        };

        assert!(args.validate().is_ok());
        assert_eq!(args.send_url().as_str(), "http://localhost:8080/");
    }

    #[test]
    fn test_empty_send_url_fails() {
        let args = AccountAbstractionArgs {
            enabled: true,
            send_url: Some("".to_string()),
            debug: false,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
        };

        assert!(matches!(
            args.validate(),
            Err(ConfigError::ValidationError(_))
        ));
    }

    #[test]
    fn test_invalid_url_fails() {
        let args = AccountAbstractionArgs {
            enabled: true,
            send_url: Some("not-a-valid-url".to_string()),
            debug: false,
            user_op_event_lookback_blocks: 1000,
            indexer_enabled: false,
        };

        assert!(matches!(args.validate(), Err(ConfigError::InvalidUrl { .. })));
    }

    #[test]
    fn test_default_lookback_blocks() {
        let args = AccountAbstractionArgs::default();
        // Default is set via clap, so in code it will be 0 unless parsed
        // This tests the getter works
        assert_eq!(args.user_op_event_lookback_blocks(), 0);
    }
}
