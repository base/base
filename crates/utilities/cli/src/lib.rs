#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod sigsegv;
pub use sigsegv::SigsegvHandler;

mod backtrace;
pub use backtrace::Backtracing;

mod metrics;
pub use metrics::MetricsArgs;

mod prometheus;
pub use prometheus::{BuildError, PrometheusServer};

mod args;
pub use args::{GlobalArgs, LogArgs};

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
