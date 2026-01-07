#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod version;
pub use version::Version;

mod sigsegv;
pub use sigsegv::SigsegvHandler;

mod backtrace;
pub use backtrace::Backtracing;

mod logs;
pub use logs::{FileLogConfig, LogConfig, LogRotation, StdoutLogConfig};

mod logging;
pub use logging::LogArgs;

mod tracing;
pub use tracing::{LogFormat, TestTracing};
