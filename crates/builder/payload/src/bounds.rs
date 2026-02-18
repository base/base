/// Trait bounds for OP Stack builder components used in payload building.
use alloy_consensus::Header;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_primitives::OpTransactionSigned;
use reth_optimism_txpool::OpPooledTx;
use reth_payload_util::PayloadTransactions;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;

/// Trait alias for transaction pool bounds required by the payload builder.
pub trait PoolBounds:
    TransactionPool<Transaction: OpPooledTx<Consensus = OpTransactionSigned>> + Unpin + 'static
where
    <Self as TransactionPool>::Transaction: OpPooledTx,
{
}

impl<T> PoolBounds for T
where
    T: TransactionPool<Transaction: OpPooledTx<Consensus = OpTransactionSigned>> + Unpin + 'static,
    <Self as TransactionPool>::Transaction: OpPooledTx,
{
}

/// Trait alias for state provider bounds required by the payload builder.
pub trait ClientBounds:
    StateProviderFactory
    + ChainSpecProvider<ChainSpec = OpChainSpec>
    + BlockReaderIdExt<Header = Header>
    + Clone
{
}

impl<T> ClientBounds for T where
    T: StateProviderFactory
        + ChainSpecProvider<ChainSpec = OpChainSpec>
        + BlockReaderIdExt<Header = Header>
        + Clone
{
}

/// Trait alias for payload transaction iterator bounds.
pub trait PayloadTxsBounds:
    PayloadTransactions<Transaction: OpPooledTx<Consensus = OpTransactionSigned>>
{
}

impl<T> PayloadTxsBounds for T where
    T: PayloadTransactions<Transaction: OpPooledTx<Consensus = OpTransactionSigned>>
{
}
