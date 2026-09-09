#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Utility functions for initializing the database.
pub mod init;

/// Various provider traits.
mod traits;
pub use traits::*;

/// Provider trait implementations.
pub mod providers;
pub use providers::{
    DatabaseProvider, DatabaseProviderRO, DatabaseProviderRW, HistoricalStateProvider,
    HistoricalStateProviderRef, LatestStateProvider, LatestStateProviderRef, LoadedJar,
    ProviderFactory, PruneShardOutcome, PrunedIndices, RocksTxIter, SaveBlocksInput,
    StaticFileAccess, StaticFileProviderBuilder, StaticFileProviderInner,
    StaticFileProviderMetrics, StaticFileWriteCtx, StaticFileWriter,
};

pub mod changeset_walker;

#[cfg(any(test, feature = "test-utils"))]
/// Common test helpers for mocking the Provider.
pub mod test_utils;

pub mod either_writer;
pub use either_writer::*;

pub mod history_shards;
pub use history_shards::{
    PreparedHistoryShardWrites, ShardedHistoryTable, prepare_history_shard_writes_parallel,
    prepare_history_shard_writes_parallel_vec, prepare_history_shard_writes_serial,
    prepare_history_shard_writes_serial_vec,
};

mod bal;
pub use bal::{BalConfig, InMemoryBalStore, RocksDBBalStore};
pub use base_execution_state_types::*;
pub use reth_chain_state::{
    CanonStateNotification, CanonStateNotificationSender, CanonStateNotificationStream,
    CanonStateNotifications, CanonStateSubscriptions,
};
// reexport traits to avoid breaking changes
/// Re-export `OriginalValuesKnown`
pub use base_execution_evm_runtime::database::OriginalValuesKnown;
pub use base_execution_state_api::{
    BalNotification, BalNotificationStream, BalProvider, BalStore, BalStoreHandle,
    GetBlockAccessListLimit, HistoryWriter, MetadataProvider, NoopBalStore, RawBal,
    StateWriteConfig, StatsReader, StorageSettings, StorageSettingsCache,
};
pub use base_execution_state_types as static_file;
/// Re-export provider error.
pub use base_execution_state_types::{ProviderError, ProviderResult};
pub use static_file::StaticFileSegment;

mod range;
pub use range::ProviderRange;

#[cfg(unix)]
mod changeset_offsets;
#[cfg(unix)]
pub use changeset_offsets::{ChangesetOffsetReader, ChangesetOffsetWriter};

mod overlay;
pub use overlay::*;
