#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub use base_cli_utils::{
    FileLogConfig, LogConfig, LogFormat, LogRotation, SigsegvHandler, StdoutLogConfig,
    init_test_tracing,
};

mod flags;
pub use flags::OverrideArgs;

#[cfg(feature = "secrets")]
mod secrets;
#[cfg(feature = "secrets")]
pub use secrets::{KeypairError, ParseKeyError, SecretKeyLoader};

pub mod backtrace;
