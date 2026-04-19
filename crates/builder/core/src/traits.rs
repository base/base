//! Trait bounds for Base builder components.

use alloy_consensus::Header;
use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{BundleTransaction, OpPooledTx, TimestampedTransaction};
use base_node_core::BaseEngineTypes;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_payload_util::PayloadTransactions;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use reth_transaction_pool::{TransactionPool, TransactionPoolExt};

/// Composite trait bound for a full node type compatible with the Base builder.
pub trait NodeBounds:
    FullNodeTypes<
    Types: NodeTypes<
        Payload = BaseEngineTypes,
        ChainSpec = BaseChainSpec,
        Primitives = BasePrimitives,
    >,
>
{
}

impl<T> NodeBounds for T where
    T: FullNodeTypes<
        Types: NodeTypes<
            Payload = BaseEngineTypes,
            ChainSpec = BaseChainSpec,
            Primitives = BasePrimitives,
        >,
    >
{
}

/// Composite trait bound for a transaction pool compatible with the Base builder.
pub trait PoolBounds:
    TransactionPool<
        Transaction: OpPooledTx<Consensus = BaseTransactionSigned>
                         + BundleTransaction
                         + TimestampedTransaction,
    > + TransactionPoolExt
    + Unpin
    + 'static
where
    <Self as TransactionPool>::Transaction: OpPooledTx + BundleTransaction + TimestampedTransaction,
{
}

impl<T> PoolBounds for T
where
    T: TransactionPool<
            Transaction: OpPooledTx<Consensus = BaseTransactionSigned>
                             + BundleTransaction
                             + TimestampedTransaction,
        > + TransactionPoolExt
        + Unpin
        + 'static,
    <Self as TransactionPool>::Transaction: OpPooledTx + BundleTransaction + TimestampedTransaction,
{
}

/// Composite trait bound for state provider clients used by the Base builder.
pub trait ClientBounds:
    StateProviderFactory
    + ChainSpecProvider<ChainSpec = BaseChainSpec>
    + BlockReaderIdExt<Header = Header>
    + Clone
{
}

impl<T> ClientBounds for T where
    T: StateProviderFactory
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone
{
}

/// Composite trait bound for payload transaction iterators used by the Base builder.
pub trait PayloadTxsBounds:
    PayloadTransactions<
    Transaction: OpPooledTx<Consensus = BaseTransactionSigned>
                     + BundleTransaction
                     + TimestampedTransaction,
>
{
}

impl<T> PayloadTxsBounds for T where
    T: PayloadTransactions<
        Transaction: OpPooledTx<Consensus = BaseTransactionSigned>
                         + BundleTransaction
                         + TimestampedTransaction,
    >
{
}
