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
                long = "metrics.enabled",
                global = true,
                default_value_t = false,
                env = concat!($prefix, "_METRICS_ENABLED")
            )]
            pub enabled: bool,

            /// The interval for prometheus metrics collection in seconds.
            #[arg(
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
            /// Increase logging verbosity (1=ERROR, 2=WARN, 3=INFO, 4=DEBUG, 5=TRACE).
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
