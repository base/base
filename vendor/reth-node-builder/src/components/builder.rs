//! A generic [`NodeComponentsBuilder`]

use std::future::Future;

use base_common_consensus::BaseTxEnvelope;
use reth_consensus::FullConsensus;
use reth_network_api::FullNetwork;
use reth_transaction_pool::{PoolTransaction, TransactionPool};

use crate::{
    BuilderContext, FullNodeTypes,
    components::{Components, NodeComponents},
};

/// Constructs the components used during node launch.
///
/// Base supplies its concrete component builder. Closures can supply components for
/// specialized launch contexts such as offline RPC services.
pub trait NodeComponentsBuilder<Node: FullNodeTypes>: Send {
    /// The components for the node with the given types
    type Components: NodeComponents<Node>;

    /// Consumes the type and returns the created components.
    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Components>> + Send;
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
    type Components = Components<Net, Pool, Cons>;

    fn build_components(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Components>> + Send {
        self(ctx)
    }
}
