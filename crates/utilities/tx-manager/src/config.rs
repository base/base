//! Transaction manager configuration.
//!
//! Two-layer configuration system: [`TxManagerCli`] captures CLI/env
//! arguments at startup, and [`TxManagerConfig`] is the validated runtime
//! configuration with hot-reloadable fee fields behind a
//! [`parking_lot::RwLock`].

use std::time::Duration;

use clap::Parser;
use parking_lot::RwLock;
use thiserror::Error;

// ── Error ───────────────────────────────────────────────────────────────

/// Errors returned during configuration validation.
#[derive(Debug, Error)]
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

    /// A gwei value failed validation (negative, `NaN`, or infinity).
    #[error("invalid gwei value for {field}: {reason}")]
    InvalidGwei {
        /// The field name that contains the invalid gwei value.
        field: &'static str,
        /// Human-readable reason the value is invalid.
        reason: String,
    },
}

// ── GweiConversion ──────────────────────────────────────────────────────

/// Converts gwei (f64) values to wei (u128).
///
/// Placed on a unit struct per project convention (prefer methods on types
/// over bare functions).
#[derive(Debug)]
pub struct GweiConversion;

impl GweiConversion {
    /// Number of wei per gwei.
    const WEI_PER_GWEI: f64 = 1_000_000_000.0;

    /// Converts a gwei value to wei.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidGwei`] if the value is negative, `NaN`,
    /// infinite, or so large that the wei result would overflow `u128`.
    pub fn gwei_to_wei(gwei: f64, field: &'static str) -> Result<u128, ConfigError> {
        if gwei.is_nan() {
            return Err(ConfigError::InvalidGwei { field, reason: "value is NaN".to_string() });
        }
        if gwei.is_infinite() {
            return Err(ConfigError::InvalidGwei {
                field,
                reason: "value is infinite".to_string(),
            });
        }
        if gwei < 0.0 {
            return Err(ConfigError::InvalidGwei {
                field,
                reason: format!("value is negative ({gwei})"),
            });
        }
        let wei = gwei * Self::WEI_PER_GWEI;
        // Guard against silent saturation: f64 values >= 2^128 would
        // saturate to u128::MAX when cast with `as u128`.
        if wei >= u128::MAX as f64 {
            return Err(ConfigError::InvalidGwei {
                field,
                reason: format!("value too large ({gwei} gwei overflows u128 wei)"),
            });
        }
        Ok(wei as u128)
    }
}

// ── FeeConfig ───────────────────────────────────────────────────────────

/// Snapshot of fee-limit parameters for deterministic fee calculations.
///
/// Extracted from [`TxManagerConfig`] hot-reloadable fields to provide a
/// consistent point-in-time view for
/// [`FeeCalculator::check_limits`](crate::FeeCalculator::check_limits).
#[derive(Debug, Clone)]
pub struct FeeConfig {
    /// Maximum allowed multiplier applied to the suggested fee.
    ///
    /// When the suggested fee is at or above [`fee_limit_threshold`](Self::fee_limit_threshold),
    /// a proposed fee that exceeds `fee_limit_multiplier × suggested` is rejected
    /// with [`TxManagerError::FeeLimitExceeded`](crate::TxManagerError::FeeLimitExceeded).
    pub fee_limit_multiplier: u64,

    /// Minimum suggested fee (in wei) at which the fee-limit check activates.
    ///
    /// If the suggested fee is below this value the limit check is skipped,
    /// allowing unconstrained fees in low-fee environments.
    pub fee_limit_threshold: u128,
}

/// Note: the default `fee_limit_threshold` is `0` (check always active),
/// which differs from the CLI default of 100 gwei. This provides a
/// minimal/permissive starting point for callers constructing a
/// [`FeeConfig`] directly rather than via [`TxManagerConfig::fee_config`].
impl Default for FeeConfig {
    fn default() -> Self {
        Self { fee_limit_multiplier: 5, fee_limit_threshold: 0 }
    }
}

// ── TxManagerPreset ─────────────────────────────────────────────────────

/// Preset default profiles for different transaction manager roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxManagerPreset {
    /// Batcher role: higher confirmation count for finality.
    Batcher,
    /// Challenger role: lower confirmation count for faster response.
    Challenger,
}

// ── TxManagerCli ────────────────────────────────────────────────────────

/// CLI arguments for the transaction manager.
///
/// Designed to be `#[command(flatten)]`-ed into parent CLI structs
/// (proposer, challenger, batcher binaries). All fields use environment
/// variable fallbacks with the `BASE_TX_MANAGER_` prefix.
#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Tx Manager")]
pub struct TxManagerCli {
    /// Number of block confirmations to wait before considering a
    /// transaction finalized.
    #[arg(
        long = "tx-manager.num-confirmations",
        env = "BASE_TX_MANAGER_NUM_CONFIRMATIONS",
        default_value = "10"
    )]
    pub num_confirmations: u64,

    /// Number of consecutive nonce-too-low errors after a successful
    /// publish before the send loop aborts.
    #[arg(
        long = "tx-manager.safe-abort-nonce-too-low-count",
        env = "BASE_TX_MANAGER_SAFE_ABORT_NONCE_TOO_LOW_COUNT",
        default_value = "3"
    )]
    pub safe_abort_nonce_too_low_count: u64,

    /// Maximum fee multiplier applied to the suggested gas price.
    #[arg(
        long = "tx-manager.fee-limit-multiplier",
        env = "BASE_TX_MANAGER_FEE_LIMIT_MULTIPLIER",
        default_value = "5"
    )]
    pub fee_limit_multiplier: u64,

    /// Minimum suggested fee (in gwei) at which the fee-limit check
    /// activates. Below this value, fees are unconstrained.
    #[arg(
        long = "tx-manager.fee-limit-threshold",
        env = "BASE_TX_MANAGER_FEE_LIMIT_THRESHOLD",
        default_value = "100.0"
    )]
    pub fee_limit_threshold_gwei: f64,

    /// Minimum tip cap (in gwei) to use for transactions.
    #[arg(
        long = "tx-manager.min-tip-cap",
        env = "BASE_TX_MANAGER_MIN_TIP_CAP",
        default_value = "0.0"
    )]
    pub min_tip_cap_gwei: f64,

    /// Minimum basefee (in gwei) to use for transactions.
    #[arg(
        long = "tx-manager.min-basefee",
        env = "BASE_TX_MANAGER_MIN_BASEFEE",
        default_value = "0.0"
    )]
    pub min_basefee_gwei: f64,

    /// Timeout for network requests (e.g., "10s", "1m").
    #[arg(
        long = "tx-manager.network-timeout",
        env = "BASE_TX_MANAGER_NETWORK_TIMEOUT",
        default_value = "10s",
        value_parser = humantime::parse_duration
    )]
    pub network_timeout: Duration,

    /// Timeout before resubmitting a transaction with bumped fees
    /// (e.g., "48s", "2m").
    #[arg(
        long = "tx-manager.resubmission-timeout",
        env = "BASE_TX_MANAGER_RESUBMISSION_TIMEOUT",
        default_value = "48s",
        value_parser = humantime::parse_duration
    )]
    pub resubmission_timeout: Duration,

    /// Interval between receipt query attempts (e.g., "12s").
    #[arg(
        long = "tx-manager.receipt-query-interval",
        env = "BASE_TX_MANAGER_RECEIPT_QUERY_INTERVAL",
        default_value = "12s",
        value_parser = humantime::parse_duration
    )]
    pub receipt_query_interval: Duration,

    /// Overall timeout for sending a transaction. Set to "0s" to disable.
    #[arg(
        long = "tx-manager.tx-send-timeout",
        env = "BASE_TX_MANAGER_TX_SEND_TIMEOUT",
        default_value = "0s",
        value_parser = humantime::parse_duration
    )]
    pub tx_send_timeout: Duration,

    /// Maximum time to wait for a transaction to appear in the mempool.
    /// Set to "0s" to disable.
    #[arg(
        long = "tx-manager.tx-not-in-mempool-timeout",
        env = "BASE_TX_MANAGER_TX_NOT_IN_MEMPOOL_TIMEOUT",
        default_value = "2m",
        value_parser = humantime::parse_duration
    )]
    pub tx_not_in_mempool_timeout: Duration,
}

impl TxManagerCli {
    /// Shared defaults used by all presets. Individual presets override
    /// only the fields that differ (e.g. `num_confirmations`).
    const fn base_defaults() -> Self {
        Self {
            num_confirmations: 10,
            safe_abort_nonce_too_low_count: 3,
            fee_limit_multiplier: 5,
            fee_limit_threshold_gwei: 100.0,
            min_tip_cap_gwei: 0.0,
            min_basefee_gwei: 0.0,
            network_timeout: Duration::from_secs(10),
            resubmission_timeout: Duration::from_secs(48),
            receipt_query_interval: Duration::from_secs(12),
            tx_send_timeout: Duration::ZERO,
            tx_not_in_mempool_timeout: Duration::from_secs(120),
        }
    }

    /// Returns a [`TxManagerCli`] populated with preset-appropriate defaults.
    ///
    /// The returned struct can be overridden by actual CLI arguments or
    /// environment variables when flattened into a parent parser.
    #[must_use]
    pub const fn with_preset(preset: TxManagerPreset) -> Self {
        let mut cli = Self::base_defaults();
        match preset {
            TxManagerPreset::Batcher => cli,
            TxManagerPreset::Challenger => {
                cli.num_confirmations = 3;
                cli
            }
        }
    }
}

// ── HotConfig ───────────────────────────────────────────────────────────

/// Hot-reloadable fee configuration fields.
///
/// Wrapped in a single [`RwLock`] inside [`TxManagerConfig`] because
/// config updates are infrequent and per-field lock granularity is
/// unnecessary.
///
/// This is intentionally kept private (not re-exported) despite the
/// workspace convention that module types should be `pub`. Exposing
/// this grouping struct would leak an internal implementation detail
/// into the crate's public API. Callers interact with the hot fields
/// through [`TxManagerConfig`] accessor/mutator methods instead.
#[derive(Debug, Clone)]
struct HotConfig {
    /// Maximum fee multiplier applied to the suggested gas price.
    fee_limit_multiplier: u64,
    /// Minimum suggested fee (in wei) at which the fee-limit check activates.
    fee_limit_threshold: u128,
    /// Minimum tip cap (in wei) to use for transactions.
    min_tip_cap: u128,
    /// Minimum basefee (in wei) to use for transactions.
    min_basefee: u128,
}

// ── TxManagerConfig ─────────────────────────────────────────────────────

/// Validated runtime configuration for the transaction manager.
///
/// Immutable fields are set once at construction. Fee-related fields are
/// hot-reloadable via accessor/mutator methods backed by a
/// [`parking_lot::RwLock`] for concurrent access.
///
/// Construct via [`TxManagerConfig::from_cli`].
pub struct TxManagerConfig {
    // ── Immutable fields ────────────────────────────────────────────
    /// Number of block confirmations to wait.
    num_confirmations: u64,
    /// Nonce-too-low abort threshold.
    safe_abort_nonce_too_low_count: u64,
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

    // ── Hot-reloadable fields ───────────────────────────────────────
    /// Fee parameters that can be updated at runtime.
    hot: RwLock<HotConfig>,
}

impl std::fmt::Debug for TxManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hot = self.hot.read();
        f.debug_struct("TxManagerConfig")
            .field("num_confirmations", &self.num_confirmations)
            .field("safe_abort_nonce_too_low_count", &self.safe_abort_nonce_too_low_count)
            .field("network_timeout", &self.network_timeout)
            .field("resubmission_timeout", &self.resubmission_timeout)
            .field("receipt_query_interval", &self.receipt_query_interval)
            .field("tx_send_timeout", &self.tx_send_timeout)
            .field("tx_not_in_mempool_timeout", &self.tx_not_in_mempool_timeout)
            .field("chain_id", &self.chain_id)
            .field("fee_limit_multiplier", &hot.fee_limit_multiplier)
            .field("fee_limit_threshold", &hot.fee_limit_threshold)
            .field("min_tip_cap", &hot.min_tip_cap)
            .field("min_basefee", &hot.min_basefee)
            .finish()
    }
}

impl Clone for TxManagerConfig {
    fn clone(&self) -> Self {
        let hot = self.hot.read().clone();
        Self {
            num_confirmations: self.num_confirmations,
            safe_abort_nonce_too_low_count: self.safe_abort_nonce_too_low_count,
            network_timeout: self.network_timeout,
            resubmission_timeout: self.resubmission_timeout,
            receipt_query_interval: self.receipt_query_interval,
            tx_send_timeout: self.tx_send_timeout,
            tx_not_in_mempool_timeout: self.tx_not_in_mempool_timeout,
            chain_id: self.chain_id,
            hot: RwLock::new(hot),
        }
    }
}

impl TxManagerConfig {
    /// Creates a validated [`TxManagerConfig`] from parsed CLI arguments
    /// and a chain ID.
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
    /// - Gwei values must not be negative, `NaN`, or infinite
    pub fn from_cli(cli: TxManagerCli, chain_id: u64) -> Result<Self, ConfigError> {
        // ── Validate integer fields ─────────────────────────────────
        if cli.num_confirmations == 0 {
            return Err(ConfigError::OutOfRange {
                field: "num_confirmations",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        if cli.safe_abort_nonce_too_low_count == 0 {
            return Err(ConfigError::OutOfRange {
                field: "safe_abort_nonce_too_low_count",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        if cli.fee_limit_multiplier == 0 {
            return Err(ConfigError::OutOfRange {
                field: "fee_limit_multiplier",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }

        // ── Validate duration fields ────────────────────────────────
        if cli.network_timeout.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "network_timeout",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }
        if cli.resubmission_timeout.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "resubmission_timeout",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }
        if cli.receipt_query_interval.is_zero() {
            return Err(ConfigError::OutOfRange {
                field: "receipt_query_interval",
                constraint: "> 0",
                value: "0s".to_string(),
            });
        }

        // ── Convert gwei to wei ─────────────────────────────────────
        let fee_limit_threshold =
            GweiConversion::gwei_to_wei(cli.fee_limit_threshold_gwei, "fee_limit_threshold")?;
        let min_tip_cap = GweiConversion::gwei_to_wei(cli.min_tip_cap_gwei, "min_tip_cap")?;
        let min_basefee = GweiConversion::gwei_to_wei(cli.min_basefee_gwei, "min_basefee")?;

        Ok(Self {
            num_confirmations: cli.num_confirmations,
            safe_abort_nonce_too_low_count: cli.safe_abort_nonce_too_low_count,
            network_timeout: cli.network_timeout,
            resubmission_timeout: cli.resubmission_timeout,
            receipt_query_interval: cli.receipt_query_interval,
            tx_send_timeout: cli.tx_send_timeout,
            tx_not_in_mempool_timeout: cli.tx_not_in_mempool_timeout,
            chain_id,
            hot: RwLock::new(HotConfig {
                fee_limit_multiplier: cli.fee_limit_multiplier,
                fee_limit_threshold,
                min_tip_cap,
                min_basefee,
            }),
        })
    }

    // ── Immutable field accessors ───────────────────────────────────

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

    // ── Hot-reloadable field accessors ──────────────────────────────

    /// Returns the current fee-limit multiplier.
    #[must_use]
    pub fn fee_limit_multiplier(&self) -> u64 {
        self.hot.read().fee_limit_multiplier
    }

    /// Returns the current fee-limit threshold (in wei).
    #[must_use]
    pub fn fee_limit_threshold(&self) -> u128 {
        self.hot.read().fee_limit_threshold
    }

    /// Returns the current minimum tip cap (in wei).
    #[must_use]
    pub fn min_tip_cap(&self) -> u128 {
        self.hot.read().min_tip_cap
    }

    /// Returns the current minimum basefee (in wei).
    #[must_use]
    pub fn min_basefee(&self) -> u128 {
        self.hot.read().min_basefee
    }

    // ── Hot-reloadable field mutators ───────────────────────────────

    /// Updates the fee-limit multiplier at runtime.
    ///
    /// Applies the same validation as [`from_cli`](Self::from_cli): the
    /// multiplier must be >= 1. A zero multiplier would compute a ceiling
    /// of zero in [`FeeCalculator::check_limits`](crate::FeeCalculator::check_limits),
    /// rejecting every transaction.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OutOfRange`] if `val` is zero.
    pub fn set_fee_limit_multiplier(&self, val: u64) -> Result<(), ConfigError> {
        if val == 0 {
            return Err(ConfigError::OutOfRange {
                field: "fee_limit_multiplier",
                constraint: ">= 1",
                value: "0".to_string(),
            });
        }
        self.hot.write().fee_limit_multiplier = val;
        Ok(())
    }

    /// Updates the fee-limit threshold (in wei) at runtime.
    ///
    /// Callers are responsible for providing a sensible value. Use
    /// [`GweiConversion::gwei_to_wei`] for gwei-to-wei conversion with
    /// validation.
    pub fn set_fee_limit_threshold(&self, val: u128) {
        self.hot.write().fee_limit_threshold = val;
    }

    /// Updates the minimum tip cap (in wei) at runtime.
    ///
    /// Callers are responsible for providing a sensible value. Use
    /// [`GweiConversion::gwei_to_wei`] for gwei-to-wei conversion with
    /// validation.
    pub fn set_min_tip_cap(&self, val: u128) {
        self.hot.write().min_tip_cap = val;
    }

    /// Updates the minimum basefee (in wei) at runtime.
    ///
    /// Callers are responsible for providing a sensible value. Use
    /// [`GweiConversion::gwei_to_wei`] for gwei-to-wei conversion with
    /// validation.
    pub fn set_min_basefee(&self, val: u128) {
        self.hot.write().min_basefee = val;
    }

    /// Returns a snapshot of the current fee configuration for use with
    /// [`FeeCalculator::check_limits`](crate::FeeCalculator::check_limits).
    #[must_use]
    pub fn fee_config(&self) -> FeeConfig {
        let hot = self.hot.read();
        FeeConfig {
            fee_limit_multiplier: hot.fee_limit_multiplier,
            fee_limit_threshold: hot.fee_limit_threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rstest::rstest;

    use super::*;

    // ── TxManagerCli defaults ───────────────────────────────────────

    #[test]
    fn cli_defaults_from_empty_args() {
        let cli = TxManagerCli::try_parse_from(["test"]).unwrap();
        assert_eq!(cli.num_confirmations, 10);
        assert_eq!(cli.safe_abort_nonce_too_low_count, 3);
        assert_eq!(cli.fee_limit_multiplier, 5);
        assert!((cli.fee_limit_threshold_gwei - 100.0).abs() < f64::EPSILON);
        assert!((cli.min_tip_cap_gwei - 0.0).abs() < f64::EPSILON);
        assert!((cli.min_basefee_gwei - 0.0).abs() < f64::EPSILON);
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
        assert!((cli.fee_limit_threshold_gwei - 200.0).abs() < f64::EPSILON);
        assert!((cli.min_tip_cap_gwei - 1.5).abs() < f64::EPSILON);
        assert!((cli.min_basefee_gwei - 0.25).abs() < f64::EPSILON);
        assert_eq!(cli.network_timeout, Duration::from_secs(30));
        assert_eq!(cli.resubmission_timeout, Duration::from_secs(60));
        assert_eq!(cli.receipt_query_interval, Duration::from_secs(5));
        assert_eq!(cli.tx_send_timeout, Duration::from_secs(120));
        assert_eq!(cli.tx_not_in_mempool_timeout, Duration::from_secs(180));
    }

    // ── Validation rejection tests ──────────────────────────────────

    fn default_cli() -> TxManagerCli {
        TxManagerCli::try_parse_from(["test"]).unwrap()
    }

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

    // ── Gwei conversion tests ───────────────────────────────────────

    #[rstest]
    #[case::zero(0.0, 0)]
    #[case::one_gwei(1.0, 1_000_000_000)]
    #[case::half_gwei(0.5, 500_000_000)]
    #[case::hundred_gwei(100.0, 100_000_000_000)]
    #[case::fractional(0.001, 1_000_000)]
    fn gwei_to_wei_valid(#[case] gwei: f64, #[case] expected_wei: u128) {
        let result = GweiConversion::gwei_to_wei(gwei, "test_field").unwrap();
        assert_eq!(result, expected_wei);
    }

    #[rstest]
    #[case::negative(-1.0, None)]
    #[case::nan(f64::NAN, Some("NaN"))]
    #[case::infinity(f64::INFINITY, Some("infinite"))]
    #[case::neg_infinity(f64::NEG_INFINITY, None)]
    #[case::very_large_finite(f64::MAX, Some("too large"))]
    #[case::borderline_large(1e30, None)]
    fn gwei_to_wei_invalid(#[case] gwei: f64, #[case] expected_substr: Option<&str>) {
        let result = GweiConversion::gwei_to_wei(gwei, "test_field");
        assert!(matches!(result, Err(ConfigError::InvalidGwei { field: "test_field", .. })));
        if let Some(substr) = expected_substr {
            let err = result.unwrap_err();
            assert!(err.to_string().contains(substr), "error should mention {substr}: {err}");
        }
    }

    // ── Invalid gwei in config construction ─────────────────────────

    #[rstest]
    #[case::negative_fee_threshold(
        TxManagerCli { fee_limit_threshold_gwei: -1.0, ..default_cli() }, "fee_limit_threshold"
    )]
    #[case::nan_min_tip_cap(
        TxManagerCli { min_tip_cap_gwei: f64::NAN, ..default_cli() }, "min_tip_cap"
    )]
    #[case::infinite_min_basefee(
        TxManagerCli { min_basefee_gwei: f64::INFINITY, ..default_cli() }, "min_basefee"
    )]
    fn invalid_gwei_in_config_rejected(#[case] cli: TxManagerCli, #[case] expected_field: &str) {
        let result = TxManagerConfig::from_cli(cli, 1);
        let err = result.expect_err("expected InvalidGwei error");
        assert!(
            matches!(&err, ConfigError::InvalidGwei { field, .. } if *field == expected_field),
            "expected InvalidGwei for {expected_field}, got: {err}"
        );
    }

    // ── Preset tests ────────────────────────────────────────────────

    #[test]
    fn batcher_preset_defaults() {
        let cli = TxManagerCli::with_preset(TxManagerPreset::Batcher);
        assert_eq!(cli.num_confirmations, 10);
        assert_eq!(cli.fee_limit_multiplier, 5);
        assert_eq!(cli.network_timeout, Duration::from_secs(10));
        assert_eq!(cli.resubmission_timeout, Duration::from_secs(48));
    }

    #[test]
    fn challenger_preset_defaults() {
        let cli = TxManagerCli::with_preset(TxManagerPreset::Challenger);
        assert_eq!(cli.num_confirmations, 3);
        assert_eq!(cli.fee_limit_multiplier, 5);
        assert_eq!(cli.network_timeout, Duration::from_secs(10));
        assert_eq!(cli.resubmission_timeout, Duration::from_secs(48));
    }

    #[test]
    fn challenger_preset_roundtrip_through_from_cli() {
        let cli = TxManagerCli::with_preset(TxManagerPreset::Challenger);
        let config = TxManagerConfig::from_cli(cli, 8453).unwrap();
        assert_eq!(config.num_confirmations(), 3);
        assert_eq!(config.chain_id(), 8453);
        assert_eq!(config.fee_limit_multiplier(), 5);
        assert_eq!(config.network_timeout(), Duration::from_secs(10));
        assert_eq!(config.resubmission_timeout(), Duration::from_secs(48));
    }

    // ── Config construction ─────────────────────────────────────────

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

    // ── Thread safety ───────────────────────────────────────────────

    #[test]
    fn tx_manager_config_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxManagerConfig>();
    }

    // ── Hot-reload tests ────────────────────────────────────────────

    #[test]
    fn hot_reload_fee_limit_multiplier() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        assert_eq!(config.fee_limit_multiplier(), 5);
        config.set_fee_limit_multiplier(10).unwrap();
        assert_eq!(config.fee_limit_multiplier(), 10);
    }

    #[test]
    fn hot_reload_fee_limit_threshold() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        config.set_fee_limit_threshold(999);
        assert_eq!(config.fee_limit_threshold(), 999);
    }

    #[test]
    fn hot_reload_min_tip_cap() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        config.set_min_tip_cap(42);
        assert_eq!(config.min_tip_cap(), 42);
    }

    #[test]
    fn hot_reload_min_basefee() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        config.set_min_basefee(123);
        assert_eq!(config.min_basefee(), 123);
    }

    // ── Hot-reload setter validation ────────────────────────────────

    #[test]
    fn set_fee_limit_multiplier_rejects_zero() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        let result = config.set_fee_limit_multiplier(0);
        assert!(matches!(
            result,
            Err(ConfigError::OutOfRange { field: "fee_limit_multiplier", .. })
        ));
        // Original value is preserved on error.
        assert_eq!(config.fee_limit_multiplier(), 5);
    }

    #[test]
    fn set_fee_limit_multiplier_accepts_boundary_one() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        config.set_fee_limit_multiplier(1).unwrap();
        assert_eq!(config.fee_limit_multiplier(), 1);
    }

    // ── Debug impl ─────────────────────────────────────────────────

    #[test]
    fn debug_impl_does_not_panic() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("TxManagerConfig"));
    }

    // ── FeeConfig snapshot ──────────────────────────────────────────

    #[test]
    fn fee_config_snapshot() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        let snapshot = config.fee_config();
        assert_eq!(snapshot.fee_limit_multiplier, 5);
        assert_eq!(snapshot.fee_limit_threshold, 100_000_000_000);

        // Mutate hot config — snapshot should be independent
        config.set_fee_limit_multiplier(99).unwrap();
        assert_eq!(snapshot.fee_limit_multiplier, 5);
        assert_eq!(config.fee_limit_multiplier(), 99);
    }

    // ── Clone captures hot state ────────────────────────────────────

    #[test]
    fn clone_captures_hot_state() {
        let config = TxManagerConfig::from_cli(default_cli(), 1).unwrap();
        config.set_fee_limit_multiplier(42).unwrap();
        let cloned = config.clone();
        assert_eq!(cloned.fee_limit_multiplier(), 42);

        // Mutations are independent after clone
        config.set_fee_limit_multiplier(100).unwrap();
        assert_eq!(cloned.fee_limit_multiplier(), 42);
        assert_eq!(config.fee_limit_multiplier(), 100);
    }

    // ── FeeConfig default ───────────────────────────────────────────

    #[test]
    fn fee_config_default() {
        let fc = FeeConfig::default();
        assert_eq!(fc.fee_limit_multiplier, 5);
        assert_eq!(fc.fee_limit_threshold, 0);
    }

    // ── Zero tx_send_timeout and tx_not_in_mempool_timeout allowed ──

    #[rstest]
    #[case::tx_send_timeout(TxManagerCli { tx_send_timeout: Duration::ZERO, ..default_cli() })]
    #[case::tx_not_in_mempool_timeout(TxManagerCli { tx_not_in_mempool_timeout: Duration::ZERO, ..default_cli() })]
    fn zero_optional_timeout_allowed(#[case] cli: TxManagerCli) {
        assert!(TxManagerConfig::from_cli(cli, 1).is_ok());
    }

    // ── ConfigError display ─────────────────────────────────────────

    #[rstest]
    #[case::out_of_range(
        ConfigError::OutOfRange { field: "num_confirmations", constraint: ">= 1", value: "0".to_string() },
        "num_confirmations must be >= 1, got 0"
    )]
    #[case::invalid_gwei(
        ConfigError::InvalidGwei { field: "min_tip_cap", reason: "value is NaN".to_string() },
        "invalid gwei value for min_tip_cap: value is NaN"
    )]
    fn config_error_display(#[case] error: ConfigError, #[case] expected: &str) {
        assert_eq!(error.to_string(), expected);
    }

    // ── Property tests ──────────────────────────────────────────────

    proptest! {
        #[test]
        fn gwei_to_wei_non_negative(gwei in 0.0..1_000_000.0f64) {
            let result = GweiConversion::gwei_to_wei(gwei, "prop_test");
            prop_assert!(result.is_ok(), "non-negative finite gwei should convert: {gwei}");
            let wei = result.unwrap();
            let expected = (gwei * 1_000_000_000.0) as u128;
            prop_assert_eq!(wei, expected);
        }
    }
}
