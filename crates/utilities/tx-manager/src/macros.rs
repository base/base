/// Generates a `TxManagerCli` struct with transaction manager CLI arguments,
/// parameterized by env var prefix at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_tx_manager::define_tx_manager_cli!("BASE_TX_MANAGER_");
/// base_tx_manager::define_tx_manager_cli!("BASE_CHALLENGER_TX_MANAGER_");
/// ```
///
/// The generated struct has eleven fields covering confirmations, fee limits,
/// timeouts, and polling intervals. Each env-backed field uses
/// `concat!($prefix, "FIELD_NAME")` — e.g., with prefix
/// `"BASE_CHALLENGER_TX_MANAGER_"` the num-confirmations field reads from
/// `BASE_CHALLENGER_TX_MANAGER_NUM_CONFIRMATIONS`.
///
/// Also generates `base_defaults()`, `with_preset(TxManagerPreset)`, and
/// `into_params(chain_id) -> Result<TxManagerParams, ConfigError>` methods.
#[rustfmt::skip]
#[macro_export]
macro_rules! define_tx_manager_cli {
    ($prefix:literal) => {
        /// CLI arguments for the transaction manager.
        ///
        /// Designed to be `#[command(flatten)]`-ed into parent CLI structs
        /// (proposer, challenger, batcher binaries). All fields use environment
        /// variable fallbacks with the configured prefix.
        ///
        /// Requires the `cli` feature.
        #[derive(Debug, Clone, ::clap::Parser)]
        #[command(next_help_heading = "Tx Manager")]
        pub struct TxManagerCli {
            /// Number of block confirmations to wait before considering a
            /// transaction finalized.
            #[arg(
                long = "tx-manager.num-confirmations",
                env = concat!($prefix, "NUM_CONFIRMATIONS"),
                default_value = "10"
            )]
            pub num_confirmations: u64,

            /// Number of consecutive nonce-too-low errors after a successful
            /// publish before the send loop aborts.
            #[arg(
                long = "tx-manager.safe-abort-nonce-too-low-count",
                env = concat!($prefix, "SAFE_ABORT_NONCE_TOO_LOW_COUNT"),
                default_value = "3"
            )]
            pub safe_abort_nonce_too_low_count: u64,

            /// Maximum fee multiplier applied to the suggested gas price.
            #[arg(
                long = "tx-manager.fee-limit-multiplier",
                env = concat!($prefix, "FEE_LIMIT_MULTIPLIER"),
                default_value = "5"
            )]
            pub fee_limit_multiplier: u64,

            /// Minimum suggested fee (in gwei) at which the fee-limit check
            /// activates. Accepts decimal strings (e.g. `"100"`, `"1.5"`).
            #[arg(
                long = "tx-manager.fee-limit-threshold",
                env = concat!($prefix, "FEE_LIMIT_THRESHOLD"),
                default_value = "100"
            )]
            pub fee_limit_threshold_gwei: String,

            /// Minimum tip cap (in gwei) to use for transactions. Accepts
            /// decimal strings (e.g. `"0"`, `"1.5"`).
            #[arg(
                long = "tx-manager.min-tip-cap",
                env = concat!($prefix, "MIN_TIP_CAP"),
                default_value = "0"
            )]
            pub min_tip_cap_gwei: String,

            /// Minimum basefee (in gwei) to use for transactions. Accepts
            /// decimal strings (e.g. `"0"`, `"0.25"`).
            #[arg(
                long = "tx-manager.min-basefee",
                env = concat!($prefix, "MIN_BASEFEE"),
                default_value = "0"
            )]
            pub min_basefee_gwei: String,

            /// Timeout for network requests (e.g., "10s", "1m").
            #[arg(
                long = "tx-manager.network-timeout",
                env = concat!($prefix, "NETWORK_TIMEOUT"),
                default_value = "10s",
                value_parser = ::humantime::parse_duration
            )]
            pub network_timeout: ::std::time::Duration,

            /// Timeout before resubmitting a transaction with bumped fees
            /// (e.g., "48s", "2m").
            #[arg(
                long = "tx-manager.resubmission-timeout",
                env = concat!($prefix, "RESUBMISSION_TIMEOUT"),
                default_value = "48s",
                value_parser = ::humantime::parse_duration
            )]
            pub resubmission_timeout: ::std::time::Duration,

            /// Interval between receipt query attempts (e.g., "12s").
            #[arg(
                long = "tx-manager.receipt-query-interval",
                env = concat!($prefix, "RECEIPT_QUERY_INTERVAL"),
                default_value = "12s",
                value_parser = ::humantime::parse_duration
            )]
            pub receipt_query_interval: ::std::time::Duration,

            /// Overall timeout for sending a transaction. Set to "0s" to disable.
            #[arg(
                long = "tx-manager.tx-send-timeout",
                env = concat!($prefix, "TX_SEND_TIMEOUT"),
                default_value = "0s",
                value_parser = ::humantime::parse_duration
            )]
            pub tx_send_timeout: ::std::time::Duration,

            /// Maximum time to wait for a transaction to appear in the mempool.
            /// Set to "0s" to disable.
            #[arg(
                long = "tx-manager.tx-not-in-mempool-timeout",
                env = concat!($prefix, "TX_NOT_IN_MEMPOOL_TIMEOUT"),
                default_value = "2m",
                value_parser = ::humantime::parse_duration
            )]
            pub tx_not_in_mempool_timeout: ::std::time::Duration,
        }

        impl TxManagerCli {
            /// Shared defaults used by all presets. Individual presets override
            /// only the fields that differ (e.g. `num_confirmations`).
            fn base_defaults() -> Self {
                <Self as ::clap::Parser>::try_parse_from(["base"])
                    .expect("hardcoded defaults are valid")
            }

            /// Returns a [`TxManagerCli`] populated with preset-appropriate defaults.
            ///
            /// The returned struct can be overridden by actual CLI arguments or
            /// environment variables when flattened into a parent parser.
            #[must_use]
            pub fn with_preset(preset: $crate::TxManagerPreset) -> Self {
                let mut cli = Self::base_defaults();
                match preset {
                    $crate::TxManagerPreset::Batcher => cli,
                    $crate::TxManagerPreset::Challenger => {
                        cli.num_confirmations = 3;
                        cli
                    }
                }
            }

            /// Converts CLI arguments into validated [`TxManagerParams`],
            /// parsing gwei strings to wei.
            ///
            /// # Errors
            ///
            /// Returns [`ConfigError`] if any gwei string is invalid.
            pub fn into_params(
                self,
                chain_id: u64,
            ) -> ::std::result::Result<$crate::TxManagerParams, $crate::ConfigError> {
                let fee_limit_threshold = $crate::GweiParser::parse(
                    &self.fee_limit_threshold_gwei,
                    "fee_limit_threshold",
                )?;
                let min_tip_cap = $crate::GweiParser::parse(
                    &self.min_tip_cap_gwei,
                    "min_tip_cap",
                )?;
                let min_basefee = $crate::GweiParser::parse(
                    &self.min_basefee_gwei,
                    "min_basefee",
                )?;

                Ok($crate::TxManagerParams {
                    num_confirmations: self.num_confirmations,
                    safe_abort_nonce_too_low_count: self.safe_abort_nonce_too_low_count,
                    fee_limit_multiplier: self.fee_limit_multiplier,
                    fee_limit_threshold,
                    min_tip_cap,
                    min_basefee,
                    network_timeout: self.network_timeout,
                    resubmission_timeout: self.resubmission_timeout,
                    receipt_query_interval: self.receipt_query_interval,
                    tx_send_timeout: self.tx_send_timeout,
                    tx_not_in_mempool_timeout: self.tx_not_in_mempool_timeout,
                    chain_id,
                })
            }
        }
    };
}
