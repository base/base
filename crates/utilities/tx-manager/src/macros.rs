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

/// Generates a `SignerCli` struct with signer CLI arguments,
/// parameterized by env var prefix at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_tx_manager::define_signer_cli!("CHALLENGER");
/// base_tx_manager::define_signer_cli!("PROPOSER");
/// ```
///
/// The generated struct has three fields covering local and remote signing.
/// Each env-backed field uses `concat!($prefix, "_", "FIELD_NAME")` — e.g.,
/// with prefix `"CHALLENGER"` the private-key field reads from
/// `CHALLENGER_PRIVATE_KEY`.
///
/// The macro also generates:
/// - Custom `Debug` impl that redacts `private_key`
/// - `impl TryFrom<SignerCli> for SignerConfig` that validates and converts
///
/// # Required downstream dependencies
///
/// The macro expands to code that references `::clap::Parser`, `::url::Url`,
/// and `::alloy_primitives::{Address, B256}` via absolute paths. Consumer
/// crates that invoke `define_signer_cli!` must add these dependencies to
/// their own `Cargo.toml`:
///
/// ```toml
/// [dependencies]
/// clap = { version = "...", features = ["derive", "env"] }
/// url = "..."
/// alloy-primitives = "..."
/// ```
#[rustfmt::skip]
#[macro_export]
macro_rules! define_signer_cli {
    ($prefix:literal) => {
        /// CLI arguments for signer configuration.
        ///
        /// Designed to be `#[command(flatten)]`-ed into parent CLI structs.
        /// Supports two mutually exclusive signing modes:
        /// - **Local**: raw private key (for dev/testing)
        /// - **Remote**: signer sidecar endpoint + address (for production)
        #[derive(Clone, ::clap::Parser)]
        #[command(next_help_heading = "Signer")]
        pub struct SignerCli {
            /// Hex-encoded secp256k1 private key (with or without 0x prefix).
            /// Mutually exclusive with --signer-endpoint.
            #[arg(
                long = "private-key",
                env = concat!($prefix, "_", "PRIVATE_KEY"),
                conflicts_with = "signer_endpoint",
            )]
            pub private_key: Option<String>,

            /// URL of the signer sidecar JSON-RPC endpoint (for production).
            /// Mutually exclusive with --private-key.
            /// Must be used together with --signer-address.
            #[arg(
                long = "signer-endpoint",
                env = concat!($prefix, "_", "SIGNER_ENDPOINT"),
                conflicts_with = "private_key",
                requires = "signer_address",
            )]
            pub signer_endpoint: Option<::url::Url>,

            /// Address of the signer account on the signer sidecar.
            /// Must be used together with --signer-endpoint.
            #[arg(
                long = "signer-address",
                env = concat!($prefix, "_", "SIGNER_ADDRESS"),
                requires = "signer_endpoint",
            )]
            pub signer_address: Option<::alloy_primitives::Address>,
        }

        impl ::std::fmt::Debug for SignerCli {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_struct("SignerCli")
                    .field(
                        "private_key",
                        &self.private_key.as_ref().map(|_| "[REDACTED]"),
                    )
                    .field("signer_endpoint", &self.signer_endpoint)
                    .field("signer_address", &self.signer_address)
                    .finish()
            }
        }

        impl TryFrom<SignerCli> for $crate::SignerConfig {
            type Error = $crate::ConfigError;

            fn try_from(cli: SignerCli) -> Result<Self, Self::Error> {
                match (cli.private_key, cli.signer_endpoint, cli.signer_address) {
                    (Some(pk), None, None) => {
                        let key: ::alloy_primitives::B256 =
                            pk.parse().map_err(|e: ::alloy_primitives::hex::FromHexError| {
                                $crate::ConfigError::InvalidValue {
                                    field: "private-key",
                                    reason: e.to_string(),
                                }
                            })?;
                        Ok($crate::SignerConfig::local(key))
                    }
                    (None, Some(endpoint), Some(address)) => {
                        if endpoint.host().is_none() {
                            return Err($crate::ConfigError::InvalidValue {
                                field: "signer-endpoint",
                                reason: "URL must have a host".to_string(),
                            });
                        }
                        Ok($crate::SignerConfig::Remote { endpoint, address })
                    }
                    (None, None, None) => Err($crate::ConfigError::InvalidValue {
                        field: "signer",
                        reason: "one of --private-key or \
                                 (--signer-endpoint + --signer-address) \
                                 must be provided"
                            .to_string(),
                    }),
                    _ => Err($crate::ConfigError::InvalidValue {
                        field: "signer",
                        reason: "invalid combination of signer arguments"
                            .to_string(),
                    }),
                }
            }
        }
    };
}
