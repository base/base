//! A generic [`NodeComponentsBuilder`]

use std::future::Future;

use base_common_consensus::BaseTxEnvelope;
use reth_consensus::FullConsensus;
use reth_network_api::FullNetwork;
use reth_transaction_pool::{PoolTransaction, TransactionPool};

use crate::{BuilderContext, FullNodeTypes, components::Components};

/// Constructs the components used during node launch.
///
/// Base supplies its concrete component builder. Closures can supply components for
/// specialized launch contexts such as offline RPC services.
pub trait NodeComponentsBuilder<Node: FullNodeTypes>: Send {
    /// Pool supplied to the node.
    type Pool: TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>>
        + Unpin
        + 'static;
    /// Consensus validator supplied to the node.
    type Consensus: FullConsensus + Clone + Unpin + 'static;
    /// Network handle supplied to the node.
    type Network: FullNetwork;

    /// Consumes the type and returns the created components.
    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<BuiltComponents<Node, Self>>> + Send;
}

impl<Node, Net, F, Fut, Pool, Cons> NodeComponentsBuilder<Node> for F
where
    Net: FullNetwork,
    Node: FullNodeTypes,
    F: FnOnce(&BuilderContext<Node>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Components<Net, Pool, Cons>>> + Send,
    Pool:
        TransactionPool<Transaction: PoolTransaction<Consensus = BaseTxEnvelope>> + Unpin + 'static,
    Cons: FullConsensus + Clone + Unpin + 'static,
{
    type Pool = Pool;
    type Consensus = Cons;
    type Network = Net;

    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<BuiltComponents<Node, Self>>> + Send {
        self(ctx)
    }
}

/// Concrete component container produced by a node builder.
pub type BuiltComponents<Node, Builder> = Components<
    <Builder as NodeComponentsBuilder<Node>>::Network,
    <Builder as NodeComponentsBuilder<Node>>::Pool,
    <Builder as NodeComponentsBuilder<Node>>::Consensus,
>;
