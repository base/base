#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

mod noop;
pub use noop::{NoopDropTimer, NoopMetric};

mod metrics;

mod inflight;
pub use inflight::InflightCounter;

#[cfg(feature = "metrics")]
mod timer;
#[cfg(feature = "metrics")]
pub use timer::DropTimer;
