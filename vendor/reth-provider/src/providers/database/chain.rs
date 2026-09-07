use base_common_consensus::BaseTxEnvelope;
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_storage_api::{BlockBodyReader, ChainStorageWriter, EmptyBodyStorage, EthStorage};

use crate::{DatabaseProvider, providers::NodeTypesForProvider};

/// Trait that provides access to implementations of [`ChainStorage`]
pub trait ChainStorage: Send + Sync {
    /// Provides access to the chain reader.
    fn reader<TX, Types>(
        &self,
    ) -> impl BlockBodyReader<DatabaseProvider<TX, Types>, Block = base_common_consensus::BaseBlock>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider;

    /// Provides access to the chain writer.
    fn writer<TX, Types>(&self) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider;
}

impl ChainStorage for EthStorage<BaseTxEnvelope, alloy_consensus::Header> {
    fn reader<TX, Types>(
        &self,
    ) -> impl BlockBodyReader<DatabaseProvider<TX, Types>, Block = base_common_consensus::BaseBlock>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider,
    {
        self
    }

    fn writer<TX, Types>(&self) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider,
    {
        self
    }
}

impl ChainStorage for EmptyBodyStorage<BaseTxEnvelope, alloy_consensus::Header> {
    fn reader<TX, Types>(
        &self,
    ) -> impl BlockBodyReader<DatabaseProvider<TX, Types>, Block = base_common_consensus::BaseBlock>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider,
    {
        self
    }

    fn writer<TX, Types>(&self) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider,
    {
        self
    }
}
