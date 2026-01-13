#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod sigsegv;
pub use sigsegv::SigsegvHandler;

mod backtrace;
pub use backtrace::Backtracing;

mod args;
pub use args::GlobalArgs;

mod prometheus;
pub use prometheus::PrometheusServer;

mod logging;
pub use logging::{LogFormat, LogRotation, LoggingArgs};

mod version;
pub use version::Version;

mod runtime;
pub use runtime::RuntimeManager;

mod styles;
pub use styles::CliStyles;
