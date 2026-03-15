#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cancellation;
pub use cancellation::Cancellation;

mod clock;
pub use clock::Clock;

mod runtime;
pub use runtime::Runtime;

mod spawner;
pub use spawner::{Spawner, TaskError, TaskHandle};

#[cfg(feature = "tokio")]
mod tokio;
#[cfg(feature = "tokio")]
pub use tokio::TokioRuntime;

#[cfg(feature = "test-utils")]
pub mod deterministic;
#[cfg(feature = "test-utils")]
pub use deterministic::DeterministicRuntime;
