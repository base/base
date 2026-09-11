/// Initializes the tracing subscriber for a binary, with optional application-specific
/// noise-suppression directives appended on top of the workspace defaults.
///
/// # Usage
///
/// ```rust,ignore
/// // Default workspace suppressions only (e.g. discv5=error):
/// base_cli_utils::init_tracing!(log_config)?;
///
/// // With additional binary-specific suppressions:
/// base_cli_utils::init_tracing!(log_config, ["libp2p_gossipsub=error"])?;
/// ```
#[macro_export]
macro_rules! init_tracing {
    ($log_config:expr) => {
        $log_config.init_tracing_subscriber()
    };
    ($log_config:expr, [$($directive:literal),+ $(,)?]) => {
        $log_config.init_tracing_subscriber_with_directives(&[$($directive),+])
    };
}

/// Generates a `MetricsArgs` struct with Prometheus metrics configuration,
/// parameterized by env var prefix and default port at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_cli_utils::define_metrics_args!("BASE_NODE", 9090);
/// base_cli_utils::define_metrics_args!("BASE_PROPOSER", 7300);
/// ```
///
/// The generated struct has four fields: `enabled`, `interval`, `port`, `addr`.
/// Each field's env var is formed by appending `_METRICS_ENABLED`, `_METRICS_INTERVAL`,
/// `_METRICS_PORT`, or `_METRICS_ADDR` to the given prefix.
#[rustfmt::skip]
#[macro_export]
macro_rules! define_metrics_args {
    ($prefix:literal, $default_port:literal) => {
        /// Configuration for Prometheus metrics.
        #[derive(Debug, Clone, ::clap::Parser, ::serde::Serialize, ::serde::Deserialize)]
        #[command(next_help_heading = "Metrics")]
        pub struct MetricsArgs {
            /// Controls whether Prometheus metrics are enabled. Disabled by default.
            #[arg(
                id = "metrics_enabled",
                long = "metrics.enabled",
                global = true,
                default_value_t = false,
                env = concat!($prefix, "_METRICS_ENABLED")
            )]
            pub enabled: bool,

            /// The interval for prometheus metrics collection in seconds.
            #[arg(
                id = "metrics_interval",
                long = "metrics.interval",
                global = true,
                default_value = "30",
                env = concat!($prefix, "_METRICS_INTERVAL")
            )]
            pub interval: u64,

            /// The port to serve Prometheus metrics on.
            #[arg(
                id = "metrics_port",
                long = "metrics.port",
                global = true,
                default_value = stringify!($default_port),
                env = concat!($prefix, "_METRICS_PORT")
            )]
            pub port: u16,

            /// The IP address to use for Prometheus metrics.
            #[arg(
                long = "metrics.addr",
                global = true,
                default_value = "0.0.0.0",
                env = concat!($prefix, "_METRICS_ADDR")
            )]
            pub addr: ::std::net::IpAddr,
        }

        impl Default for MetricsArgs {
            fn default() -> Self {
                Self {
                    enabled: false,
                    interval: 30,
                    port: $default_port,
                    addr: ::std::net::IpAddr::V4(::std::net::Ipv4Addr::UNSPECIFIED),
                }
            }
        }

        impl From<MetricsArgs> for $crate::MetricsConfig {
            fn from(args: MetricsArgs) -> Self {
                Self {
                    enabled: args.enabled,
                    interval: args.interval,
                    addr: args.addr,
                    port: args.port,
                }
            }
        }
    };
}

/// Generates a `TelemetryArgs` struct with node telemetry configuration,
/// parameterized by env var prefix at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_cli_utils::define_telemetry_args!("BASE_NODE");
/// ```
///
/// Telemetry is opt-*out*: `enabled` defaults to `true` and is switched off with
/// `--telemetry.enabled=false`. Reporting is nevertheless inert until an endpoint is configured,
/// so a build with no `--telemetry.endpoint` sends nothing and mints no identity.
///
/// Each env-backed field appends `_TELEMETRY_ENABLED`, `_TELEMETRY_ENDPOINT`,
/// `_TELEMETRY_INSTANCE_ID`, `_TELEMETRY_ID_PATH`, `_TELEMETRY_DATA_DIR`,
/// `_TELEMETRY_REPORT_INTERVAL`, or `_TELEMETRY_SAMPLE_INTERVAL` to the given prefix.
///
/// Also generates `impl Default for TelemetryArgs` and
/// `TelemetryArgs::config(&self, l2_chain_id)`, which resolves the identity path for the chain
/// when `--telemetry.id-path` is not set.
#[rustfmt::skip]
#[macro_export]
macro_rules! define_telemetry_args {
    ($prefix:literal) => {
        /// Configuration for node telemetry reporting.
        ///
        /// Telemetry is opt-out. Switch it off with `--telemetry.enabled=false`.
        #[derive(Debug, Clone, ::clap::Parser)]
        #[command(next_help_heading = "Telemetry")]
        pub struct TelemetryArgs {
            /// Controls whether this node reports telemetry. Enabled by default; reporting still
            /// requires an endpoint.
            #[arg(
                id = "telemetry_enabled",
                long = "telemetry.enabled",
                global = true,
                default_value_t = true,
                action = ::clap::ArgAction::Set,
                env = concat!($prefix, "_TELEMETRY_ENABLED")
            )]
            pub enabled: bool,

            /// Where to send telemetry reports. Nothing is sent while this is unset.
            #[arg(
                id = "telemetry_endpoint",
                long = "telemetry.endpoint",
                global = true,
                env = concat!($prefix, "_TELEMETRY_ENDPOINT")
            )]
            pub endpoint: Option<::url::Url>,

            /// Operator-chosen tag for this node, used to identify a node across restarts.
            #[arg(
                id = "telemetry_instance_id",
                long = "telemetry.instance-id",
                global = true,
                env = concat!($prefix, "_TELEMETRY_INSTANCE_ID")
            )]
            pub instance_id: Option<String>,

            /// Where the persisted telemetry identity lives.
            ///
            /// Defaults to `$HOME/.base/<l2_chain_id>/telemetry-id`. Set this when `$HOME` is
            /// unset, as it is for most containers: with neither, there is nowhere durable to
            /// keep an identity and the node reports nothing.
            #[arg(
                id = "telemetry_id_path",
                long = "telemetry.id-path",
                global = true,
                env = concat!($prefix, "_TELEMETRY_ID_PATH")
            )]
            pub id_path: Option<::std::path::PathBuf>,

            /// Directory whose filesystem the reported disk fields describe.
            ///
            /// Defaults to the directory holding the node's own on-disk state. Set this when
            /// chain data lives on a volume the node does not otherwise name, or the disk
            /// fields describe the wrong device.
            #[arg(
                id = "telemetry_data_dir",
                long = "telemetry.data-dir",
                global = true,
                env = concat!($prefix, "_TELEMETRY_DATA_DIR")
            )]
            pub data_dir: Option<::std::path::PathBuf>,

            /// How often to send a report, in seconds.
            #[arg(
                id = "telemetry_report_interval",
                long = "telemetry.report-interval",
                global = true,
                default_value = "900",
                env = concat!($prefix, "_TELEMETRY_REPORT_INTERVAL")
            )]
            pub report_interval: u64,

            /// How often to sample head lag between reports, in seconds.
            #[arg(
                id = "telemetry_sample_interval",
                long = "telemetry.sample-interval",
                global = true,
                default_value = "60",
                env = concat!($prefix, "_TELEMETRY_SAMPLE_INTERVAL")
            )]
            pub sample_interval: u64,
        }

        impl Default for TelemetryArgs {
            fn default() -> Self {
                Self {
                    enabled: true,
                    endpoint: None,
                    instance_id: None,
                    id_path: None,
                    data_dir: None,
                    report_interval: 900,
                    sample_interval: 60,
                }
            }
        }

        impl TelemetryArgs {
            /// Resolves these arguments into a telemetry client configuration.
            ///
            /// `l2_chain_id` only decides where the identity is persisted, and only when
            /// `--telemetry.id-path` is not set.
            ///
            /// The identity path resolves to `None` when neither the flag nor `$HOME` names a
            /// location. That is the whole answer: the node warns and reports nothing, rather
            /// than writing an identity to a working directory it may not have next restart.
            pub fn config(&self, l2_chain_id: u64) -> $crate::TelemetryConfig {
                // An empty value counts as unset. A declared-but-empty
                // `<PREFIX>_TELEMETRY_ID_PATH` is how a container manifest routinely spells "not
                // configured", and clap hands that through as an empty path rather than `None`.
                let id_path = self
                    .id_path
                    .clone()
                    .filter(|path| !path.as_os_str().is_empty())
                    .or_else(|| $crate::TelemetryConfig::default_id_path(l2_chain_id));
                $crate::TelemetryConfig {
                    enabled: self.enabled,
                    endpoint: self.endpoint.clone(),
                    instance_id: self.instance_id.clone(),
                    data_dir: self.data_dir.clone(),
                    report_interval: ::std::time::Duration::from_secs(self.report_interval),
                    sample_interval: ::std::time::Duration::from_secs(self.sample_interval),
                    ..$crate::TelemetryConfig::disabled(id_path)
                }
            }
        }
    };
}

/// Generates a `LogArgs` struct with logging configuration,
/// parameterized by env var prefix at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_cli_utils::define_log_args!("BASE_PROPOSER");
/// ```
///
/// The generated struct has six fields covering verbosity, stdout quiet mode,
/// stdout format, file log directory, file format, and file rotation.
/// Each env-backed field uses the given prefix: `_LOG_VERBOSITY`, `_LOG_FORMAT`,
/// and `_LOG_DIR`.
///
/// Note: `_LOG_VERBOSITY` expects a **numeric** value (1=ERROR … 5=TRACE), not a
/// level string like `"info"`. The name is intentionally distinct from
/// `_LOG_LEVEL` to prevent that mistake.
///
/// Also generates `impl Default for LogArgs` and `impl From<LogArgs> for LogConfig`.
#[rustfmt::skip]
#[macro_export]
macro_rules! define_log_args {
    ($prefix:literal) => {
        /// Log-related CLI arguments.
        ///
        /// Verbosity levels: 1=ERROR, 2=WARN, 3=INFO (default), 4=DEBUG, 5=TRACE.
        /// Use `-q` to suppress stdout logging entirely.
        #[derive(Debug, Clone, ::clap::Parser, ::serde::Serialize, ::serde::Deserialize)]
        #[command(next_help_heading = "Logging")]
        pub struct LogArgs {
            /// Set logging verbosity: no flag=INFO, -v=ERROR, -vv=WARN, -vvv=INFO,
            /// -vvvv=DEBUG, -vvvvv=TRACE.
            #[arg(
                short = 'v',
                long = "verbose",
                action = ::clap::ArgAction::Count,
                default_value = "3",
                env = concat!($prefix, "_LOG_VERBOSITY"),
                global = true
            )]
            pub level: u8,

            /// Suppress stdout logging.
            #[arg(long = "logs.stdout.quiet", alias = "quiet", short = 'q', global = true)]
            pub stdout_quiet: bool,

            /// Stdout log format.
            #[arg(
                long = "logs.stdout.format",
                default_value = "full",
                env = concat!($prefix, "_LOG_FORMAT"),
                global = true
            )]
            pub stdout_format: $crate::LogFormat,

            /// Directory for file logging (enables file logging when set).
            #[arg(long = "logs.file.directory", env = concat!($prefix, "_LOG_DIR"), global = true)]
            pub file_directory: Option<::std::path::PathBuf>,

            /// File log format.
            #[arg(long = "logs.file.format", default_value = "json", global = true)]
            pub file_format: $crate::LogFormat,

            /// File log rotation strategy.
            #[arg(long = "logs.file.rotation", default_value = "never", global = true)]
            pub file_rotation: $crate::LogRotation,
        }

        impl Default for LogArgs {
            fn default() -> Self {
                Self {
                    level: 3,
                    stdout_quiet: false,
                    stdout_format: $crate::LogFormat::Full,
                    file_directory: None,
                    file_format: $crate::LogFormat::Json,
                    file_rotation: $crate::LogRotation::Never,
                }
            }
        }

        impl From<LogArgs> for $crate::LogConfig {
            fn from(args: LogArgs) -> Self {
                let stdout_logs = if args.stdout_quiet {
                    None
                } else {
                    Some($crate::StdoutLogConfig { format: args.stdout_format })
                };
                let file_logs = args.file_directory.map(|dir| $crate::FileLogConfig {
                    directory_path: dir,
                    format: args.file_format,
                    rotation: args.file_rotation,
                });
                Self {
                    global_level: $crate::verbosity_to_level_filter(args.level),
                    stdout_logs,
                    file_logs,
                }
            }
        }
    };
}

/// Generates a `HealthArgs` struct with health server configuration,
/// parameterized by env var prefix and default port at compile time.
///
/// # Usage
///
/// ```rust,ignore
/// base_cli_utils::define_health_args!("BASE_CHALLENGER", 8080);
/// base_cli_utils::define_health_args!("BASE_PROPOSER", 8080);
/// ```
///
/// The generated struct has two fields: `addr` and `port`.
/// Each field's env var is formed by appending `_HEALTH_ADDR` or `_HEALTH_PORT`
/// to the given prefix.
///
/// A convenience method `socket_addr()` returns a `SocketAddr` from the two fields.
#[rustfmt::skip]
#[macro_export]
macro_rules! define_health_args {
    ($prefix:literal, $default_port:literal) => {
        /// Configuration for the health-check HTTP server.
        #[derive(Debug, Clone, ::clap::Parser)]
        #[command(next_help_heading = "Health Server")]
        pub struct HealthArgs {
            /// Health server bind address.
            #[arg(
                id = "health_addr",
                long = "health.addr",
                default_value = "0.0.0.0",
                env = concat!($prefix, "_HEALTH_ADDR")
            )]
            pub addr: ::std::net::IpAddr,

            /// Health server port.
            #[arg(
                id = "health_port",
                long = "health.port",
                default_value = stringify!($default_port),
                env = concat!($prefix, "_HEALTH_PORT")
            )]
            pub port: u16,
        }

        impl Default for HealthArgs {
            fn default() -> Self {
                Self {
                    addr: ::std::net::IpAddr::V4(::std::net::Ipv4Addr::UNSPECIFIED),
                    port: $default_port,
                }
            }
        }

        impl HealthArgs {
            /// Returns the configured socket address.
            pub const fn socket_addr(&self) -> ::std::net::SocketAddr {
                ::std::net::SocketAddr::new(self.addr, self.port)
            }
        }
    };
}

/// Generates a local `cli_env!` macro that prepends a fixed component prefix to
/// every env-var suffix, so you can write `env = cli_env!("L1_ETH_RPC")` instead
/// of `env = "BASE_CHALLENGER_L1_ETH_RPC"`.
///
/// # Usage
///
/// ```rust,ignore
/// base_cli_utils::define_cli_env!("BASE_CHALLENGER");
///
/// #[arg(long = "l1-eth-rpc", env = cli_env!("L1_ETH_RPC"))]
/// pub l1_eth_rpc: Url,
/// // expands to env = "BASE_CHALLENGER_L1_ETH_RPC"
/// ```
#[macro_export]
macro_rules! define_cli_env {
    ($prefix:literal) => {
        $crate::define_cli_env!(@dollar $prefix $);
    };
    (@dollar $prefix:literal $d:tt) => {
        macro_rules! cli_env {
            ($d var:literal) => {
                concat!($prefix, "_", $d var)
            };
        }
    };
}
