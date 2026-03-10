#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

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
