//! Helper provider traits to encapsulate all provider traits for simplicity.

use std::fmt::Debug;

use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_chain_state::{
    CanonStateSubscriptions, ForkChoiceSubscriptions, PersistedBlockSubscriptions,
};
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_storage_api::{
    StorageChangeSetReader, StorageSettingsCache, TryIntoHistoricalStateProvider,
};

use crate::{
    BalProvider, BlockReader, BlockReaderIdExt, ChainSpecProvider, ChangeSetReader,
    DatabaseProviderFactory, PruneCheckpointReader, RocksDBProviderFactory, StageCheckpointReader,
    StateProviderFactory, StateRangeProviderFactory, StateReader, StaticFileProviderFactory,
};

/// Helper trait to unify all provider traits for simplicity.
pub trait FullProvider<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>:
    DatabaseProviderFactory<
        DB = DB,
        Provider: BlockReader
                      + StageCheckpointReader
                      + PruneCheckpointReader
                      + ChangeSetReader
                      + StorageChangeSetReader
                      + StorageSettingsCache
                      + TryIntoHistoricalStateProvider
                      + 'static,
    > + StaticFileProviderFactory
    + RocksDBProviderFactory
    + BlockReaderIdExt<Transaction = BaseTxEnvelope, Block = BaseBlock, Receipt = BaseReceipt>
    + BalProvider
    + StateProviderFactory
    + StateRangeProviderFactory
    + StateReader
    + ChainSpecProvider
    + ChangeSetReader
    + StorageChangeSetReader
    + CanonStateSubscriptions
    + ForkChoiceSubscriptions
    + PersistedBlockSubscriptions
    + StageCheckpointReader
    + PruneCheckpointReader
    + Clone
    + Debug
    + Unpin
    + 'static
{
}

impl<T, DB: Database + DatabaseMetrics + Clone + Unpin + 'static> FullProvider<DB> for T where
    T: DatabaseProviderFactory<
            DB = DB,
            Provider: BlockReader
                          + StageCheckpointReader
                          + PruneCheckpointReader
                          + ChangeSetReader
                          + StorageChangeSetReader
                          + StorageSettingsCache
                          + TryIntoHistoricalStateProvider
                          + 'static,
        > + StaticFileProviderFactory
        + RocksDBProviderFactory
        + BlockReaderIdExt<Transaction = BaseTxEnvelope, Block = BaseBlock, Receipt = BaseReceipt>
        + BalProvider
        + StateProviderFactory
        + StateRangeProviderFactory
        + StateReader
        + ChainSpecProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + CanonStateSubscriptions
        + ForkChoiceSubscriptions
        + PersistedBlockSubscriptions
        + StageCheckpointReader
        + PruneCheckpointReader
        + Clone
        + Debug
        + Unpin
        + 'static
{
}
