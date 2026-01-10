#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod args;
mod logging;
mod version;

pub use args::GlobalArgs;
pub use logging::{LogFormat, LogRotation, LoggingArgs};
pub use version::Version;

pub mod runtime;
pub use runtime::{build_runtime, run_until_ctrl_c, run_until_ctrl_c_fallible};
