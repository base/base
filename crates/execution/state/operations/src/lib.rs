#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod api;
pub use api::{
    BaseProofsBatchSession, BaseProofsBatchStore, BaseProofsInitialStateStore, BaseProofsStore,
    BlockStateDiff, StorageBranchEntries,
};

pub mod initialize;
pub use initialize::InitializationJob;

pub mod in_memory;
pub use in_memory::{
    InMemoryAccountCursor, InMemoryBatchSession, InMemoryProofsStorage, InMemoryStorageCursor,
    InMemoryTrieCursor,
};

pub mod db;
pub use db::{
    MdbxAccountCursor, MdbxBatchSession, MdbxProofsStorage, MdbxProofsStorageOptions,
    MdbxStorageCursor, MdbxTrieCursor, ProofWindowValue, RocksDbHistoryTable,
    RocksDbLatestVersionResult, RocksdbAccountCursor, RocksdbBatchSession,
    RocksdbHistoryDeleteBatch, RocksdbPreparedHistoryDeletes, RocksdbPreparedPrune,
    RocksdbProofsStorage, RocksdbProofsStorageOptions, RocksdbPrunePlan, RocksdbReadSnapshot,
    RocksdbReplacementState, RocksdbStorageCursor, RocksdbTrieCursor, RocksdbVersionedCursor,
};

pub mod metrics;
#[cfg(feature = "metrics")]
pub use metrics::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsStorage,
    BaseProofsTrieCursor, StorageMetrics,
};

#[cfg(not(feature = "metrics"))]
/// Alias for [`BaseProofsStore`] type without metrics (`metrics` feature is disabled).
pub type BaseProofsStorage<S> = S;

pub mod proof;

pub mod provider;

mod progress;
pub use progress::{ProofsProgress, ProofsProgressError};

mod batch_provider;
pub use batch_provider::BaseProofsBatchStateProviderRef;

pub mod live;

pub mod cursor;
#[cfg(not(feature = "metrics"))]
pub use cursor::{
    BaseProofsHashedAccountCursor, BaseProofsHashedStorageCursor, BaseProofsTrieCursor,
};

pub mod cursor_factory;
pub use cursor_factory::{
    BaseProofsBatchHashedAccountCursorFactory, BaseProofsBatchTrieCursorFactory,
    BaseProofsHashedAccountCursorFactory, BaseProofsTrieCursorFactory,
};

pub mod error;
pub use error::{BaseProofsStorageError, BaseProofsStorageResult};

mod prune;
pub use prune::{
    BaseProofStoragePruner, BaseProofStoragePrunerResult, BaseProofStoragePrunerTask,
    ProofStoragePrunerError, ProofStoragePrunerOutput,
};

mod proof_task;
pub use proof_task::*;

#[cfg(feature = "metrics")]
mod proof_task_metrics;
#[cfg(feature = "metrics")]
pub use proof_task_metrics::*;

mod state_root_error;
pub use state_root_error::*;

mod value_encoder;
pub use value_encoder::*;

mod state_root_task;
pub use state_root_task::*;

mod cached_state;
pub use cached_state::*;

mod txpool_cache;
pub use txpool_cache::TxPoolPrewarmCacheSnapshot;

mod payload_cache;
pub use payload_cache::PayloadExecutionCache;

mod maintenance;
pub use maintenance::*;
