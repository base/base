/// Generates a `TxManagerCli` struct with transaction manager CLI arguments,
/// parameterized by env var prefix at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_tx_manager::define_tx_manager_cli!("BASE_TX_MANAGER");
/// base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER_TX_MANAGER");
/// ```
///
/// The generated struct exposes confirmations, fee limits, timeouts, polling
/// intervals, and blob fee controls. Each env-backed field uses
/// `concat!($prefix, "_", "FIELD_NAME")` — e.g., with prefix
/// `"BASE_CHALLENGER_TX_MANAGER"` the num-confirmations field reads from
/// `BASE_CHALLENGER_TX_MANAGER_NUM_CONFIRMATIONS`.
///
/// The macro also generates:
/// - `impl Default for TxManagerCli` derived from clap's default values
/// - `impl TryFrom<TxManagerCli> for TxManagerConfig` that parses gwei fields
///   and validates the result
///
/// # Required downstream dependencies
///
/// The macro expands to code that references `::clap::Parser` and
/// `::humantime::parse_duration` via absolute paths. Consumer crates that
/// invoke `define_tx_manager_cli!` must add these dependencies to their own
/// `Cargo.toml`:
///
/// ```toml
/// [dependencies]
/// clap = { version = "...", features = ["derive", "env"] }
/// humantime = "..."
/// ```
#[rustfmt::skip]
#[macro_export]
macro_rules! define_tx_manager_cli {
    ($prefix:literal) => {
        /// CLI arguments for the transaction manager.
        ///
        /// Designed to be `#[command(flatten)]`-ed into parent CLI structs
        /// (proposer, challenger, batcher binaries). All fields use environment
        /// variable fallbacks with the configured prefix.
        #[derive(Debug, Clone, ::clap::Parser)]
        #[command(next_help_heading = "Tx Manager")]
        pub struct TxManagerCli {
            /// Number of block confirmations to wait before considering a
            /// transaction finalized.
            #[arg(
                long = "tx-manager.num-confirmations",
                env = concat!($prefix, "_", "NUM_CONFIRMATIONS"),
                default_value = "10"
            )]
            pub num_confirmations: u64,

            /// Number of consecutive nonce-too-low errors after a successful
            /// publish before the send loop aborts.
            #[arg(
                long = "tx-manager.safe-abort-nonce-too-low-count",
                env = concat!($prefix, "_", "SAFE_ABORT_NONCE_TOO_LOW_COUNT"),
                default_value = "3"
            )]
            pub safe_abort_nonce_too_low_count: u64,

            /// Maximum fee multiplier applied to the suggested gas price.
            #[arg(
                long = "tx-manager.fee-limit-multiplier",
                env = concat!($prefix, "_", "FEE_LIMIT_MULTIPLIER"),
                default_value = "5"
            )]
            pub fee_limit_multiplier: u64,

            /// Minimum suggested fee (in gwei) at which the fee-limit check
            /// activates. Accepts decimal strings (e.g. `"100"`, `"1.5"`).
            #[arg(
                long = "tx-manager.fee-limit-threshold",
                env = concat!($prefix, "_", "FEE_LIMIT_THRESHOLD"),
                default_value = "100"
            )]
            pub fee_limit_threshold_gwei: String,

            /// Minimum tip cap (in gwei) to use for transactions. Accepts
            /// decimal strings (e.g. `"0"`, `"1.5"`).
            #[arg(
                long = "tx-manager.min-tip-cap",
                env = concat!($prefix, "_", "MIN_TIP_CAP"),
                default_value = "0"
            )]
            pub min_tip_cap_gwei: String,

            /// Minimum basefee (in gwei) to use for transactions. Accepts
            /// decimal strings (e.g. `"0"`, `"0.25"`).
            #[arg(
                long = "tx-manager.min-basefee",
                env = concat!($prefix, "_", "MIN_BASEFEE"),
                default_value = "0"
            )]
            pub min_basefee_gwei: String,

            /// Timeout for network requests (e.g., "10s", "1m").
            #[arg(
                long = "tx-manager.network-timeout",
                env = concat!($prefix, "_", "NETWORK_TIMEOUT"),
                default_value = "10s",
                value_parser = ::humantime::parse_duration
            )]
            pub network_timeout: ::std::time::Duration,

            /// Timeout before resubmitting a transaction with bumped fees
            /// (e.g., "48s", "2m").
            #[arg(
                long = "tx-manager.resubmission-timeout",
                env = concat!($prefix, "_", "RESUBMISSION_TIMEOUT"),
                default_value = "48s",
                value_parser = ::humantime::parse_duration
            )]
            pub resubmission_timeout: ::std::time::Duration,

            /// Interval between receipt query attempts (e.g., "12s").
            #[arg(
                long = "tx-manager.receipt-query-interval",
                env = concat!($prefix, "_", "RECEIPT_QUERY_INTERVAL"),
                default_value = "12s",
                value_parser = ::humantime::parse_duration
            )]
            pub receipt_query_interval: ::std::time::Duration,

            /// Overall timeout for sending a transaction. Set to "0s" to disable.
            #[arg(
                long = "tx-manager.tx-send-timeout",
                env = concat!($prefix, "_", "TX_SEND_TIMEOUT"),
                default_value = "0s",
                value_parser = ::humantime::parse_duration
            )]
            pub tx_send_timeout: ::std::time::Duration,

            /// Maximum time to wait for a transaction to appear in the mempool.
            /// Set to "0s" to disable.
            #[arg(
                long = "tx-manager.tx-not-in-mempool-timeout",
                env = concat!($prefix, "_", "TX_NOT_IN_MEMPOOL_TIMEOUT"),
                default_value = "2m",
                value_parser = ::humantime::parse_duration
            )]
            pub tx_not_in_mempool_timeout: ::std::time::Duration,

            /// Maximum time to poll for transaction confirmation before giving
            /// up (e.g., "5m", "300s").
            #[arg(
                long = "tx-manager.confirmation-timeout",
                env = concat!($prefix, "_", "CONFIRMATION_TIMEOUT"),
                default_value = "5m",
                value_parser = ::humantime::parse_duration
            )]
            pub confirmation_timeout: ::std::time::Duration,

            /// Minimum blob base fee (in gwei) to use for blob transactions.
            /// Accepts decimal strings (e.g. `"1"`, `"0.5"`).
            #[arg(
                long = "tx-manager.min-blob-fee",
                env = concat!($prefix, "_", "MIN_BLOB_FEE"),
                default_value = "1"
            )]
            pub min_blob_fee_gwei: String,

        }

        impl Default for TxManagerCli {
            fn default() -> Self {
                Self::try_parse_from(["default"]).expect("clap default values are valid")
            }
        }

        impl TryFrom<TxManagerCli> for $crate::TxManagerConfig {
            type Error = $crate::ConfigError;

            fn try_from(cli: TxManagerCli) -> Result<Self, Self::Error> {
                let fee_limit_threshold =
                    $crate::GweiParser::parse(&cli.fee_limit_threshold_gwei, "fee_limit_threshold")?;
                let min_tip_cap =
                    $crate::GweiParser::parse(&cli.min_tip_cap_gwei, "min_tip_cap")?;
                let min_basefee =
                    $crate::GweiParser::parse(&cli.min_basefee_gwei, "min_basefee")?;
                let min_blob_fee =
                    $crate::GweiParser::parse(&cli.min_blob_fee_gwei, "min_blob_fee")?;

                let config = $crate::TxManagerConfig {
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
                    confirmation_timeout: cli.confirmation_timeout,
                    min_blob_fee,
                };
                config.validate()?;
                Ok(config)
            }
        }
    };
}
