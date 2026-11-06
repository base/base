//! Trait bounds for Base builder components.

use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
use base_execution_chainspec::BaseChainSpec;
use base_execution_txpool::{BasePooledTx, StateDiffInvalidation, TimestampedTransaction};
use base_node_core::BaseEngineTypes;
use reth_node_api::{FullNodeTypes, NodeTypes};
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
        Transaction: BasePooledTx<Consensus = BaseTransactionSigned> + TimestampedTransaction,
    > + TransactionPoolExt
    + base_execution_txpool::ParkableTransactionPool
    + StateDiffInvalidation
    + Unpin
    + 'static
where
    <Self as TransactionPool>::Transaction: BasePooledTx + TimestampedTransaction,
{
}

impl<T> PoolBounds for T
where
    T: TransactionPool<
            Transaction: BasePooledTx<Consensus = BaseTransactionSigned> + TimestampedTransaction,
        > + TransactionPoolExt
        + base_execution_txpool::ParkableTransactionPool
        + StateDiffInvalidation
        + Unpin
        + 'static,
    <Self as TransactionPool>::Transaction: BasePooledTx + TimestampedTransaction,
{
}
