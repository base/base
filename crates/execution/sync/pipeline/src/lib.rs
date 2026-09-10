#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[expect(missing_docs)]
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// Implementations of stages.
mod stages;
pub use stages::*;

mod sets;
pub use sets::{
    DefaultStages, ExecutionStages, HashingStages, HistoryIndexingStages, OfflineStages,
    OnlineStages,
};

mod error;
pub use error::*;

mod metrics;
pub use metrics::*;

mod pipeline;
pub use pipeline::*;

mod stage;
pub use stage::*;

mod util;

use aquamarine as _;
pub use base_execution_state_types::*;
