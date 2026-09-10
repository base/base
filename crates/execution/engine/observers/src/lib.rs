#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod backfill;
pub use backfill::*;

mod context;
pub use context::*;

mod event;
pub use event::*;

mod manager;
pub use manager::*;

mod notifications;
pub use notifications::*;

mod wal;
// Re-export exex types
#[doc(inline)]
pub use base_execution_engine_types::{ExExHead, ExExNotification, ExExNotificationBincode};
pub use wal::*;

mod proof_history;
pub use proof_history::{
    BaseProofsExEx, BaseProofsExExBuilder, CachedBlockTrieData, SyncTarget, SyncTargetState,
};

/// Fixtures for observer unit tests.
#[cfg(test)]
pub mod test_utils;
