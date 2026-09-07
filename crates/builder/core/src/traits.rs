//! Trait bounds for Base builder components.

use base_common_consensus::BaseTransactionSigned;
use base_execution_payload_builder::ParkablePayloadTransactions;
use base_execution_txpool::{BasePooledTx, StateDiffInvalidation, TimestampedTransaction};
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use reth_transaction_pool::{TransactionPool, TransactionPoolExt};

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

/// Composite trait bound for state provider clients used by the Base builder.
pub trait ClientBounds:
    StateProviderFactory + ChainSpecProvider + BlockReaderIdExt + Clone
{
}

impl<T> ClientBounds for T where
    T: StateProviderFactory + ChainSpecProvider + BlockReaderIdExt + Clone
{
}

/// Composite trait bound for payload transaction iterators used by the Base builder.
pub trait PayloadTxsBounds:
    ParkablePayloadTransactions<
    Transaction: BasePooledTx<Consensus = BaseTransactionSigned> + TimestampedTransaction,
>
{
}

impl<T> PayloadTxsBounds for T where
    T: ParkablePayloadTransactions<
        Transaction: BasePooledTx<Consensus = BaseTransactionSigned> + TimestampedTransaction,
    >
{
}
