//! Contains the main provider types and traits for interacting with the blockchain's storage.

use reth_chainspec::EthereumHardforks;
use reth_node_types::{NodeTypes, NodeTypesWithDB};

mod database;
pub use database::*;

mod static_file;
pub use static_file::{
    StaticFileAccess, StaticFileJarProvider, StaticFileProvider, StaticFileProviderBuilder,
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
    RocksDBRawIter, RocksDBStats, RocksDBTableStats, RocksReadSnapshot, RocksTx,
};

/// Helper trait to bound [`NodeTypes`] so that combined with database they satisfy
/// [`ProviderNodeTypes`].
pub trait NodeTypesForProvider
where
    Self: NodeTypes<ChainSpec: EthereumHardforks, Storage: ChainStorage>,
{
}

impl<T> NodeTypesForProvider for T where
    T: NodeTypes<ChainSpec: EthereumHardforks, Storage: ChainStorage>
{
}

/// Helper trait keeping common requirements of providers for [`NodeTypesWithDB`].
pub trait ProviderNodeTypes
where
    Self: NodeTypesForProvider + NodeTypesWithDB,
{
}
impl<T> ProviderNodeTypes for T where T: NodeTypesForProvider + NodeTypesWithDB {}
