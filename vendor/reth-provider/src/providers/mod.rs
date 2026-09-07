//! Contains the main provider types and traits for interacting with the blockchain's storage.

mod database;
pub use database::*;

mod static_file;
pub use static_file::{
    LoadedJar, StaticFileAccess, StaticFileJarProvider, StaticFileProvider,
    StaticFileProviderBuilder, StaticFileProviderInner, StaticFileProviderMetrics,
    StaticFileProviderRW, StaticFileProviderRWRefMut, StaticFileWriteCtx, StaticFileWriter,
};

mod state;
pub use state::{
    historical::{
        HistoricalStateProvider, HistoricalStateProviderRef, HistoryInfo, LowestAvailableBlocks,
        compute_history_rank, history_info, needs_prev_shard_check,
    },
    latest::{LatestStateProvider, LatestStateProviderRef},
};

mod blockchain_provider;
pub use blockchain_provider::{BlockchainProvider, SNAPSHOT_STATE_RETENTION};

mod consistent;
pub use consistent::ConsistentProvider;

pub(crate) mod rocksdb;
pub use rocksdb::{
    PruneShardOutcome, PrunedIndices, RocksDBBatch, RocksDBBuilder, RocksDBIter, RocksDBProvider,
    RocksDBRawIter, RocksDBStats, RocksDBTableStats, RocksReadSnapshot, RocksTx, RocksTxIter,
};
