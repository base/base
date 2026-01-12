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

mod logging;
pub use logging::{LogFormat, LogRotation, LoggingArgs};

mod version;
pub use version::Version;

mod runtime;
pub use runtime::{build_runtime, run_until_ctrl_c, run_until_ctrl_c_fallible};

mod styles;
pub use styles::CliStyles;
