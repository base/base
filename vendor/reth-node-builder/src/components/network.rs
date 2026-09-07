//! Network component for the node builder.

use std::future::Future;

use base_common_consensus::{BaseBlock, BaseReceipt};
use reth_network::types::NetworkPrimitives;
use reth_network_api::FullNetwork;
use reth_transaction_pool::TransactionPool;

use crate::{BuilderContext, FullNodeTypes};

/// A type that knows how to build the network implementation.
pub trait NetworkBuilder<Node: FullNodeTypes, Pool: TransactionPool>: Send {
    /// The network built.
    type Network: FullNetwork<
        Primitives: NetworkPrimitives<
            BlockHeader = alloy_consensus::Header,
            BlockBody = base_common_consensus::BaseBlockBody,
            Block = BaseBlock,
            Receipt = BaseReceipt,
        >,
    >;

    /// Launches the network implementation and returns the handle to it.
    fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> impl Future<Output = eyre::Result<Self::Network>> + Send;
}

impl<Node, Net, F, Fut, Pool> NetworkBuilder<Node, Pool> for F
where
    Node: FullNodeTypes,
    Net: FullNetwork<
        Primitives: NetworkPrimitives<
            BlockHeader = alloy_consensus::Header,
            BlockBody = base_common_consensus::BaseBlockBody,
            Block = BaseBlock,
            Receipt = BaseReceipt,
        >,
    >,
    Pool: TransactionPool,
    F: Fn(&BuilderContext<Node>, Pool) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Net>> + Send,
{
    type Network = Net;

    fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> impl Future<Output = eyre::Result<Net>> + Send {
        self(ctx, pool)
    }
}
