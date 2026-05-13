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

mod api;
pub use api::{
    BaseProofsInitialStateStore, BaseProofsStore, BlockStateDiff, InitialStateAnchor,
    InitialStateStatus, OperationDurations, WriteCounts,
};

mod initialize;
pub use initialize::InitializationJob;

mod in_memory;
pub use in_memory::{
    InMemoryAccountCursor, InMemoryProofsStorage, InMemoryStorageCursor, InMemoryTrieCursor,
};

mod db;
pub use db::*;

mod metrics;
#[cfg(feature = "metrics")]
pub use metrics::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsStorage,
    BaseProofsTrieCursor,
};
pub use metrics::{
    BaseProofsHashedCursorWithMetrics, BaseProofsStorageWithMetrics,
    BaseProofsTrieCursorWithMetrics, BlockMetrics, StorageMetrics, StorageOperation,
};

#[cfg(not(feature = "metrics"))]
/// Alias for [`BaseProofsStore`] type without metrics (`metrics` feature is disabled).
pub type BaseProofsStorage<S> = S;

mod proof;
pub use proof::{
    DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
    DatabaseTrieWitness,
};

mod provider;
pub use provider::BaseProofsStateProviderRef;

mod live;
pub use live::LiveTrieCollector;

mod cursor;
#[cfg(feature = "metrics")]
pub use cursor::{
    BaseProofsHashedAccountCursor as BaseProofsRawHashedAccountCursor,
    BaseProofsHashedStorageCursor as BaseProofsRawHashedStorageCursor,
    BaseProofsTrieCursor as BaseProofsRawTrieCursor,
};
#[cfg(not(feature = "metrics"))]
pub use cursor::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsTrieCursor,
};

mod cursor_factory;
pub use cursor_factory::{BaseProofsHashedAccountCursorFactory, BaseProofsTrieCursorFactory};

mod error;
pub use error::{BaseProofsStorageError, BaseProofsStorageResult};

mod prune;
pub use prune::{
    BaseProofStoragePruner, BaseProofStoragePrunerResult, BaseProofStoragePrunerTask, PrunerError,
    PrunerOutput,
};
