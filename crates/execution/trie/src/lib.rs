#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

// Used only for feature propagation (serde-bincode-compat workaround).
#[cfg(feature = "serde-bincode-compat")]
use reth_ethereum_primitives as _;

pub mod api;
pub use api::{BlockStateDiff, OpProofsInitialStateStore, OpProofsStore};

pub mod initialize;
pub use initialize::InitializationJob;

pub mod in_memory;
pub use in_memory::{
    InMemoryAccountCursor, InMemoryProofsStorage, InMemoryStorageCursor, InMemoryTrieCursor,
};

pub mod db;
pub use db::{MdbxAccountCursor, MdbxProofsStorage, MdbxStorageCursor, MdbxTrieCursor};

#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(not(feature = "metrics"))]
#[allow(missing_docs)]
/// No-op metrics shims exported for non-metrics builds.
pub mod metrics {
    /// No-op metric handle used when the `metrics` feature is disabled.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct NoopMetric;

    impl NoopMetric {
        #[inline(always)]
        pub fn set<T>(&self, _: T) {}

        #[inline(always)]
        pub fn increment<T>(&self, _: T) {}

        #[inline(always)]
        pub fn record<T>(&self, _: T) {}
    }

    /// No-op block metrics accessors used when the `metrics` feature is disabled.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct BlockMetrics;

    impl BlockMetrics {
        #[inline(always)]
        pub fn total_duration_seconds() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn execution_duration_seconds() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn state_root_duration_seconds() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn write_duration_seconds() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn account_trie_updates_written_total() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn storage_trie_updates_written_total() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn hashed_accounts_written_total() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn hashed_storages_written_total() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn earliest_number() -> NoopMetric {
            NoopMetric
        }

        #[inline(always)]
        pub fn latest_number() -> NoopMetric {
            NoopMetric
        }
    }
}
#[cfg(feature = "metrics")]
pub use metrics::{
    OpProofsHashedAccountCursor, OpProofsHashedStorageCursor, OpProofsStorage, OpProofsTrieCursor,
    StorageMetrics,
};

#[cfg(not(feature = "metrics"))]
/// Alias for [`OpProofsStore`] type without metrics (`metrics` feature is disabled).
pub type OpProofsStorage<S> = S;

pub mod proof;

pub mod provider;

pub mod live;

pub mod cursor;
#[cfg(not(feature = "metrics"))]
pub use cursor::{OpProofsHashedAccountCursor, OpProofsHashedStorageCursor, OpProofsTrieCursor};

pub mod cursor_factory;
pub use cursor_factory::{OpProofsHashedAccountCursorFactory, OpProofsTrieCursorFactory};

pub mod error;
pub use error::{OpProofsStorageError, OpProofsStorageResult};

mod prune;
pub use prune::{
    OpProofStoragePruner, OpProofStoragePrunerResult, OpProofStoragePrunerTask, PrunerError,
    PrunerOutput,
};
