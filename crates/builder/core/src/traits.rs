//! Trait bounds for Base builder components.

use base_execution_payload_builder::ParkablePayloadTransactions;
use base_execution_txpool::{
    BasePooledTransaction, StateDiffInvalidation, TransactionPool, TransactionPoolExt,
};
use base_execution_state_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};

/// Composite trait bound for a transaction pool compatible with the Base builder.
pub trait PoolBounds:
    TransactionPool
    + TransactionPoolExt
    + base_execution_txpool::ParkableTransactionPool
    + StateDiffInvalidation
    + Unpin
    + 'static
{
}

impl<T> PoolBounds for T where
    T: TransactionPool
        + TransactionPoolExt
        + base_execution_txpool::ParkableTransactionPool
        + StateDiffInvalidation
        + Unpin
        + 'static
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
    ParkablePayloadTransactions<Transaction = BasePooledTransaction>
{
}

impl<T> PayloadTxsBounds for T where
    T: ParkablePayloadTransactions<Transaction = BasePooledTransaction>
{
}
