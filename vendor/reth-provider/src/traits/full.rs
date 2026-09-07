//! Helper provider traits to encapsulate all provider traits for simplicity.

use std::fmt::Debug;

use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_chain_state::{
    CanonStateSubscriptions, ForkChoiceSubscriptions, PersistedBlockSubscriptions,
};
use reth_node_types::NodeTypesWithDB;
use reth_storage_api::{
    StorageChangeSetReader, StorageSettingsCache, TryIntoHistoricalStateProvider,
};

use crate::{
    BalProvider, BlockReader, BlockReaderIdExt, ChainSpecProvider, ChangeSetReader,
    DatabaseProviderFactory, PruneCheckpointReader, RocksDBProviderFactory, StageCheckpointReader,
    StateProviderFactory, StateRangeProviderFactory, StateReader, StaticFileProviderFactory,
};

/// Helper trait to unify all provider traits for simplicity.
pub trait FullProvider<N: NodeTypesWithDB>:
    DatabaseProviderFactory<
        DB = N::DB,
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
    + BlockReaderIdExt<
        Transaction = BaseTxEnvelope,
        Block = BaseBlock,
        Receipt = BaseReceipt,
        Header = alloy_consensus::Header,
    > + BalProvider
    + StateProviderFactory
    + StateRangeProviderFactory
    + StateReader
    + ChainSpecProvider<ChainSpec = N::ChainSpec>
    + ChangeSetReader
    + StorageChangeSetReader
    + CanonStateSubscriptions
    + ForkChoiceSubscriptions<Header = alloy_consensus::Header>
    + PersistedBlockSubscriptions
    + StageCheckpointReader
    + PruneCheckpointReader
    + Clone
    + Debug
    + Unpin
    + 'static
{
}

impl<T, N: NodeTypesWithDB> FullProvider<N> for T where
    T: DatabaseProviderFactory<
            DB = N::DB,
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
        + BlockReaderIdExt<
            Transaction = BaseTxEnvelope,
            Block = BaseBlock,
            Receipt = BaseReceipt,
            Header = alloy_consensus::Header,
        > + BalProvider
        + StateProviderFactory
        + StateRangeProviderFactory
        + StateReader
        + ChainSpecProvider<ChainSpec = N::ChainSpec>
        + ChangeSetReader
        + StorageChangeSetReader
        + CanonStateSubscriptions
        + ForkChoiceSubscriptions<Header = alloy_consensus::Header>
        + PersistedBlockSubscriptions
        + StageCheckpointReader
        + PruneCheckpointReader
        + Clone
        + Debug
        + Unpin
        + 'static
{
}
