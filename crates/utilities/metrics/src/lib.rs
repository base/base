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

mod define_metrics;
mod timing_macros;

mod inflight;
pub use inflight::InflightCounter;

#[cfg(feature = "metrics")]
mod registry;
#[cfg(feature = "metrics")]
#[doc(hidden)]
pub use ctor as __private_ctor;
#[cfg(feature = "metrics")]
pub use registry::initialize_registered_metrics;
#[cfg(feature = "metrics")]
#[doc(hidden)]
pub use registry::register_initializer;
#[cfg(not(feature = "metrics"))]
#[inline(always)]
/// Initializes all registered metrics. No-op when the `metrics` feature is disabled.
pub fn initialize_registered_metrics() {}

#[cfg(feature = "metrics")]
mod timer;
#[cfg(feature = "metrics")]
pub use timer::DropTimer;
