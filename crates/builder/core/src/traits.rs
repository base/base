//! Trait bounds for OP Stack builder components.

use alloy_consensus::Header;
use base_execution_chainspec::OpChainSpec;
use base_execution_primitives::{OpPrimitives, OpTransactionSigned};
use base_execution_txpool::OpPooledTx;
use base_node_core::OpEngineTypes;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_payload_util::PayloadTransactions;
use reth_provider::{BlockReaderIdExt, ChainSpecProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;

pub trait NodeBounds:
    FullNodeTypes<
    Types: NodeTypes<Payload = OpEngineTypes, ChainSpec = OpChainSpec, Primitives = OpPrimitives>,
>
{
}

impl<T> NodeBounds for T where
    T: FullNodeTypes<
        Types: NodeTypes<
            Payload = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >
{
}

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

pub trait PayloadTxsBounds:
    PayloadTransactions<Transaction: OpPooledTx<Consensus = OpTransactionSigned>>
{
}

impl<T> PayloadTxsBounds for T where
    T: PayloadTransactions<Transaction: OpPooledTx<Consensus = OpTransactionSigned>>
{
}
