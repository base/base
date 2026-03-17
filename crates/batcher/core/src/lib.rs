#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::BatchDriverError;

mod outcome;
pub use outcome::TxOutcome;

mod throttle;
pub use throttle::{
    DaThrottle, ThrottleConfig, ThrottleController, ThrottleParams, ThrottleStrategy,
};

mod throttle_client;
pub use throttle_client::{NoopThrottleClient, ThrottleClient};

mod submissions;
pub use submissions::SubmissionQueue;

mod config;
pub use config::BatchDriverConfig;

mod event;
pub use event::DriverEvent;

mod driver;
pub use driver::BatchDriver;

pub mod test_utils;
