//! Transaction manager configuration.
//!
//! [`TxManagerConfig`] is the validated runtime configuration for the
//! transaction manager. Construct it via [`TxManagerConfig::from_cli`]
//! (requires the `cli` feature), which parses CLI/env arguments and
//! validates them into the runtime configuration.

use std::time::Duration;

use alloy_primitives::utils::{UnitsError, parse_units};
use thiserror::Error;

#[cfg(feature = "cli")]
use crate::TxManagerCli;

// ── Error ───────────────────────────────────────────────────────────────

/// Errors returned during configuration validation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
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

    /// A gwei string could not be parsed or represents an invalid value.
    #[error("invalid gwei value for {field}: {source}")]
    InvalidGwei {
        /// The field name that contains the invalid gwei value.
        field: &'static str,
        /// The underlying parsing error.
        source: UnitsError,
    },
}

// ── GweiParser ─────────────────────────────────────────────────────────

/// Parses gwei decimal strings to wei (`u128`) via
/// [`alloy_primitives::utils::parse_units`].
///
/// Placed on a unit struct per project convention (prefer methods on types
/// over bare functions).
#[derive(Debug)]
pub struct GweiParser;

impl GweiParser {
    /// Parses a gwei decimal string to wei (`u128`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidGwei`] if the string is not a valid
    /// decimal number, represents a negative value, or overflows `u128`.
    pub fn parse(gwei: &str, field: &'static str) -> Result<u128, ConfigError> {
        let parsed =
            parse_units(gwei, "gwei").map_err(|e| ConfigError::InvalidGwei { field, source: e })?;
        if parsed.is_negative() {
            return Err(ConfigError::InvalidGwei {
                field,
                source: UnitsError::InvalidUnit(format!("negative value: {gwei}")),
            });
        }
        u128::try_from(parsed).map_err(|_| ConfigError::InvalidGwei {
            field,
            source: UnitsError::InvalidUnit(format!("value too large: {gwei}")),
        })
    }
}

// ── TxManagerConfig ─────────────────────────────────────────────────────

/// Validated runtime configuration for the transaction manager.
///
/// Construct via [`TxManagerConfig::from_cli`] (requires the `cli` feature).
#[derive(Debug, Clone)]
pub struct TxManagerConfig {
    /// Number of block confirmations to wait.
    num_confirmations: u64,
    /// Nonce-too-low abort threshold.
    safe_abort_nonce_too_low_count: u64,
    /// Maximum fee multiplier applied to the suggested gas price.
    fee_limit_multiplier: u64,
    /// Minimum suggested fee (in wei) at which the fee-limit check activates.
    fee_limit_threshold: u128,
    /// Minimum tip cap (in wei) to use for transactions.
    min_tip_cap: u128,
    /// Minimum basefee (in wei) to use for transactions.
    min_basefee: u128,
    /// Network request timeout.
    network_timeout: Duration,
    /// Fee-bump resubmission timeout.
    resubmission_timeout: Duration,
    /// Receipt polling interval.
    receipt_query_interval: Duration,
    /// Overall send timeout (zero = disabled).
    tx_send_timeout: Duration,
    /// Mempool appearance timeout (zero = disabled).
    tx_not_in_mempool_timeout: Duration,
    /// Chain ID for the target network.
    chain_id: u64,
}

impl TxManagerConfig {
    /// Validates the configuration fields.
    #[cfg(feature = "cli")]
    fn validate(&self) -> Result<(), ConfigError> {
        if self.num_confirmations == 0 {
            return Err(ConfigError::OutOfRange {
                field: "num_confirmations",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        if self.safe_abort_nonce_too_low_count == 0 {
            return Err(ConfigError::OutOfRange {
                field: "safe_abort_nonce_too_low_count",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        if self.fee_limit_multiplier == 0 {
            return Err(ConfigError::OutOfRange {
                field: "fee_limit_multiplier",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        if self.network_timeout.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "network_timeout",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }
        if self.resubmission_timeout.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "resubmission_timeout",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }
        if self.receipt_query_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "receipt_query_interval",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }
        Ok(())
    }

    /// Creates a validated [`TxManagerConfig`] from parsed CLI arguments
    /// and a chain ID.
    ///
    /// Parses gwei strings from the CLI struct and validates all fields.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if any validation check fails:
    /// - `num_confirmations` must be >= 1
    /// - `safe_abort_nonce_too_low_count` must be >= 1
    /// - `fee_limit_multiplier` must be >= 1
    /// - `network_timeout` must be > 0
    /// - `resubmission_timeout` must be > 0
    /// - `receipt_query_interval` must be > 0
    /// - Gwei strings must be valid non-negative decimals
    #[cfg(feature = "cli")]
    pub fn from_cli(cli: TxManagerCli, chain_id: u64) -> Result<Self, ConfigError> {
        let fee_limit_threshold =
            GweiParser::parse(&cli.fee_limit_threshold_gwei, "fee_limit_threshold")?;
        let min_tip_cap = GweiParser::parse(&cli.min_tip_cap_gwei, "min_tip_cap")?;
        let min_basefee = GweiParser::parse(&cli.min_basefee_gwei, "min_basefee")?;

        let config = Self {
            num_confirmations: cli.num_confirmations,
            safe_abort_nonce_too_low_count: cli.safe_abort_nonce_too_low_count,
            fee_limit_multiplier: cli.fee_limit_multiplier,
            fee_limit_threshold,
            min_tip_cap,
            min_basefee,
            network_timeout: cli.network_timeout,
            resubmission_timeout: cli.resubmission_timeout,
            receipt_query_interval: cli.receipt_query_interval,
            tx_send_timeout: cli.tx_send_timeout,
            tx_not_in_mempool_timeout: cli.tx_not_in_mempool_timeout,
            chain_id,
        };
        config.validate()?;
        Ok(config)
    }

    // ── Field accessors ────────────────────────────────────────────

    /// Returns the number of block confirmations required.
    #[must_use]
    pub const fn num_confirmations(&self) -> u64 {
        self.num_confirmations
    }

    /// Returns the nonce-too-low abort threshold.
    #[must_use]
    pub const fn safe_abort_nonce_too_low_count(&self) -> u64 {
        self.safe_abort_nonce_too_low_count
    }

    /// Returns the network request timeout.
    #[must_use]
    pub const fn network_timeout(&self) -> Duration {
        self.network_timeout
    }

    /// Returns the fee-bump resubmission timeout.
    #[must_use]
    pub const fn resubmission_timeout(&self) -> Duration {
        self.resubmission_timeout
    }

    /// Returns the receipt polling interval.
    #[must_use]
    pub const fn receipt_query_interval(&self) -> Duration {
        self.receipt_query_interval
    }

    /// Returns the overall send timeout (zero means disabled).
    #[must_use]
    pub const fn tx_send_timeout(&self) -> Duration {
        self.tx_send_timeout
    }

    /// Returns the mempool appearance timeout (zero means disabled).
    #[must_use]
    pub const fn tx_not_in_mempool_timeout(&self) -> Duration {
        self.tx_not_in_mempool_timeout
    }

    /// Returns the chain ID.
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    // ── Fee field accessors ─────────────────────────────────────────

    /// Returns the fee-limit multiplier.
    #[must_use]
    pub const fn fee_limit_multiplier(&self) -> u64 {
        self.fee_limit_multiplier
    }

    /// Returns the fee-limit threshold (in wei).
    #[must_use]
    pub const fn fee_limit_threshold(&self) -> u128 {
        self.fee_limit_threshold
    }

    /// Returns the minimum tip cap (in wei).
    #[must_use]
    pub const fn min_tip_cap(&self) -> u128 {
        self.min_tip_cap
    }

    /// Returns the minimum basefee (in wei).
    #[must_use]
    pub const fn min_basefee(&self) -> u128 {
        self.min_basefee
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;

    use super::*;

    // ── GweiParser tests ───────────────────────────────────────────

    #[rstest]
    #[case::zero("0", 0)]
    #[case::one_gwei("1", 1_000_000_000)]
    #[case::one_point_zero("1.0", 1_000_000_000)]
    #[case::half_gwei("0.5", 500_000_000)]
    #[case::hundred_gwei("100", 100_000_000_000)]
    #[case::fractional("0.001", 1_000_000)]
    #[case::nine_decimals("1.123456789", 1_123_456_789)]
    fn gwei_parse_valid(#[case] gwei: &str, #[case] expected_wei: u128) {
        let result = GweiParser::parse(gwei, "test_field").unwrap();
        assert_eq!(result, expected_wei);
    }

    #[rstest]
    #[case::negative("-1", "negative")]
    #[case::abc("abc", "test_field")]
    #[case::spaces("  ", "test_field")]
    fn gwei_parse_invalid(#[case] gwei: &str, #[case] expected_substr: &str) {
        let result = GweiParser::parse(gwei, "test_field");
        assert!(matches!(result, Err(ConfigError::InvalidGwei { field: "test_field", .. })));
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains(expected_substr),
            "error should mention {expected_substr}: {err}"
        );
    }

    // ── ConfigError display ─────────────────────────────────────────

    #[rstest]
    #[case::out_of_range(
        ConfigError::OutOfRange { field: "num_confirmations", constraint: ">= 1", value: "0".to_string() },
        "num_confirmations must be >= 1, got 0"
    )]
    #[case::invalid_gwei(
        ConfigError::InvalidGwei { field: "min_tip_cap", source: UnitsError::InvalidUnit("negative value: -1".to_string()) },
        "invalid gwei value for min_tip_cap: "
    )]
    fn config_error_display(#[case] error: ConfigError, #[case] expected: &str) {
        let msg = error.to_string();
        assert!(msg.contains(expected), "expected display to contain {expected:?}, got: {msg}");
    }

    // ── Thread safety ───────────────────────────────────────────────

    #[test]
    fn tx_manager_config_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxManagerConfig>();
    }

    // ── Property tests ──────────────────────────────────────────────

    proptest! {
        #[test]
        fn gwei_parse_non_negative(whole in 0u64..1_000_000, frac in 0u32..1_000_000_000) {
            let gwei_str = format!("{whole}.{frac:09}");
            let result = GweiParser::parse(&gwei_str, "prop_test");
            prop_assert!(result.is_ok(), "valid decimal string should parse: {gwei_str}");
        }
    }

    // ── CLI-dependent tests ─────────────────────────────────────────

    #[cfg(feature = "cli")]
    mod cli_tests {
        use clap::Parser;
        use rstest::rstest;

        use super::super::*;
        use crate::TxManagerCli;

        fn default_cli() -> TxManagerCli {
            TxManagerCli::try_parse_from(["test"]).unwrap()
        }

        // ── CLI defaults ────────────────────────────────────────────

        #[test]
        fn cli_defaults_from_empty_args() {
            let cli = TxManagerCli::try_parse_from(["test"]).unwrap();
            assert_eq!(cli.num_confirmations, 10);
            assert_eq!(cli.safe_abort_nonce_too_low_count, 3);
            assert_eq!(cli.fee_limit_multiplier, 5);
            assert_eq!(cli.fee_limit_threshold_gwei, "100");
            assert_eq!(cli.min_tip_cap_gwei, "0");
            assert_eq!(cli.min_basefee_gwei, "0");
            assert_eq!(cli.network_timeout, Duration::from_secs(10));
            assert_eq!(cli.resubmission_timeout, Duration::from_secs(48));
            assert_eq!(cli.receipt_query_interval, Duration::from_secs(12));
            assert_eq!(cli.tx_send_timeout, Duration::ZERO);
            assert_eq!(cli.tx_not_in_mempool_timeout, Duration::from_secs(120));
        }

        #[test]
        fn cli_parses_explicit_flags() {
            let cli = TxManagerCli::try_parse_from([
                "test",
                "--tx-manager.num-confirmations",
                "5",
                "--tx-manager.fee-limit-multiplier",
                "10",
                "--tx-manager.safe-abort-nonce-too-low-count",
                "7",
                "--tx-manager.fee-limit-threshold",
                "200.0",
                "--tx-manager.min-tip-cap",
                "1.5",
                "--tx-manager.min-basefee",
                "0.25",
                "--tx-manager.network-timeout",
                "30s",
                "--tx-manager.resubmission-timeout",
                "1m",
                "--tx-manager.receipt-query-interval",
                "5s",
                "--tx-manager.tx-send-timeout",
                "2m",
                "--tx-manager.tx-not-in-mempool-timeout",
                "3m",
            ])
            .unwrap();
            assert_eq!(cli.num_confirmations, 5);
            assert_eq!(cli.fee_limit_multiplier, 10);
            assert_eq!(cli.safe_abort_nonce_too_low_count, 7);
            assert_eq!(cli.fee_limit_threshold_gwei, "200.0");
            assert_eq!(cli.min_tip_cap_gwei, "1.5");
            assert_eq!(cli.min_basefee_gwei, "0.25");
            assert_eq!(cli.network_timeout, Duration::from_secs(30));
            assert_eq!(cli.resubmission_timeout, Duration::from_secs(60));
            assert_eq!(cli.receipt_query_interval, Duration::from_secs(5));
            assert_eq!(cli.tx_send_timeout, Duration::from_secs(120));
            assert_eq!(cli.tx_not_in_mempool_timeout, Duration::from_secs(180));
        }

        // ── Validation rejection via from_cli ───────────────────────

        #[rstest]
        #[case::num_confirmations(
            TxManagerCli { num_confirmations: 0, ..default_cli() }, "num_confirmations"
        )]
        #[case::safe_abort_nonce_too_low_count(
            TxManagerCli { safe_abort_nonce_too_low_count: 0, ..default_cli() }, "safe_abort_nonce_too_low_count"
        )]
        #[case::fee_limit_multiplier(
            TxManagerCli { fee_limit_multiplier: 0, ..default_cli() }, "fee_limit_multiplier"
        )]
        #[case::network_timeout(
            TxManagerCli { network_timeout: Duration::ZERO, ..default_cli() }, "network_timeout"
        )]
        #[case::resubmission_timeout(
            TxManagerCli { resubmission_timeout: Duration::ZERO, ..default_cli() }, "resubmission_timeout"
        )]
        #[case::receipt_query_interval(
            TxManagerCli { receipt_query_interval: Duration::ZERO, ..default_cli() }, "receipt_query_interval"
        )]
        fn zero_value_rejected(#[case] cli: TxManagerCli, #[case] expected_field: &str) {
            let result = TxManagerConfig::from_cli(cli, 1);
            let err = result.expect_err("expected OutOfRange error");
            assert!(
                matches!(&err, ConfigError::OutOfRange { field, .. } if *field == expected_field),
                "expected OutOfRange for {expected_field}, got: {err}"
            );
        }

        // ── Invalid gwei in config construction ─────────────────────

        #[rstest]
        #[case::negative_fee_threshold(
            TxManagerCli { fee_limit_threshold_gwei: "-1".to_string(), ..default_cli() }, "fee_limit_threshold"
        )]
        #[case::invalid_min_tip_cap(
            TxManagerCli { min_tip_cap_gwei: "abc".to_string(), ..default_cli() }, "min_tip_cap"
        )]
        #[case::invalid_min_basefee(
            TxManagerCli { min_basefee_gwei: "not_a_number".to_string(), ..default_cli() }, "min_basefee"
        )]
        fn invalid_gwei_in_config_rejected(
            #[case] cli: TxManagerCli,
            #[case] expected_field: &str,
        ) {
            let result = TxManagerConfig::from_cli(cli, 1);
            let err = result.expect_err("expected InvalidGwei error");
            assert!(
                matches!(&err, ConfigError::InvalidGwei { field, .. } if *field == expected_field),
                "expected InvalidGwei for {expected_field}, got: {err}"
            );
        }

        // ── from_cli valid ──────────────────────────────────────────

        #[test]
        fn from_cli_valid() {
            let cli = default_cli();
            let config = TxManagerConfig::from_cli(cli, 42).unwrap();
            assert_eq!(config.num_confirmations(), 10);
            assert_eq!(config.safe_abort_nonce_too_low_count(), 3);
            assert_eq!(config.chain_id(), 42);
            assert_eq!(config.fee_limit_multiplier(), 5);
            assert_eq!(config.fee_limit_threshold(), 100_000_000_000); // 100 gwei
            assert_eq!(config.min_tip_cap(), 0);
            assert_eq!(config.min_basefee(), 0);
            assert_eq!(config.network_timeout(), Duration::from_secs(10));
            assert_eq!(config.resubmission_timeout(), Duration::from_secs(48));
            assert_eq!(config.receipt_query_interval(), Duration::from_secs(12));
            assert_eq!(config.tx_send_timeout(), Duration::ZERO);
            assert_eq!(config.tx_not_in_mempool_timeout(), Duration::from_secs(120));
        }

        // ── Zero optional timeouts allowed via from_cli ─────────────

        #[rstest]
        #[case::tx_send_timeout(TxManagerCli { tx_send_timeout: Duration::ZERO, ..default_cli() })]
        #[case::tx_not_in_mempool_timeout(TxManagerCli { tx_not_in_mempool_timeout: Duration::ZERO, ..default_cli() })]
        fn zero_optional_timeout_allowed(#[case] cli: TxManagerCli) {
            assert!(TxManagerConfig::from_cli(cli, 1).is_ok());
        }
    }
}
