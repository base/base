#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod allocator;
#[cfg(all(feature = "jemalloc", unix))]
pub use allocator::tikv_jemalloc_sys;
pub use allocator::{Allocator, new_allocator};
mod cancellation;
pub use cancellation::{CancellationGuard, CancellationToken};

/// Helper function to load a secret key from a file.
mod load_secret_key;
pub use load_secret_key::{
    SecretKeyError, get_secret_key, parse_secret_key_from_hex, rng_secret_key,
};

/// Cli parsers functions.
mod parsers;
pub use parsers::{
    SocketAddressParsingError, format_duration_as_secs_or_ms, hash_or_num_value_parser,
    parse_duration_from_secs, parse_duration_from_secs_or_ms, parse_ether_value,
    parse_socket_address, read_json_from_file,
};

mod sigsegv;
pub use sigsegv::SigsegvHandler;

mod backtrace;
pub use backtrace::Backtracing;

mod prometheus;
pub use prometheus::{BuildError, MetricsConfig, PrometheusServer};

mod styles;
pub use styles::CliStyles;

mod logging;
pub use logging::{
    FileLogConfig, LogConfig, LogFormat, LogLevel, LogRotation, StdoutLogConfig,
    verbosity_to_level_filter,
};

mod tracing;
pub use tracing::{LogfmtFormatter, init_test_tracing};

mod version;
pub use version::Version;

mod logs_dir;
pub use logs_dir::LogsDir;

mod cli;

mod runtime;
pub use runtime::RuntimeManager;

#[macro_use]
mod macros;

mod runner;
pub use runner::{
    CliContext, CliRunner, CliRunnerConfig, cli_context, run_to_completion_or_panic,
    run_until_ctrl_c, runtime_shutdown,
};

mod chainspec;
pub use chainspec::parse_genesis;

mod trace_args;
pub use trace_args::{DefaultTraceValues, OtlpInitStatus, OtlpLogsStatus, TraceArgs};
