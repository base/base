//! A generic [`NodeComponentsBuilder`]

use std::future::Future;

use base_common_consensus::BaseTxEnvelope;
use reth_consensus::{FullConsensus, noop::NoopConsensus};
use reth_evm::BaseEvmConfig;
use reth_network_api::{FullNetwork, noop::NoopNetwork};
use reth_payload_builder::PayloadBuilderHandle;
use reth_transaction_pool::{PoolTransaction, TransactionPool};

use crate::{
    BuilderContext, FullNodeTypes,
    components::{
        Components, ConsensusBuilder, NetworkBuilder, NodeComponents, PayloadServiceBuilder,
    },
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

/// Builds [`NoopNetwork`].
#[derive(Debug, Clone, Default)]
pub struct NoopNetworkBuilder;

impl<N, Pool> NetworkBuilder<N, Pool> for NoopNetworkBuilder
where
    N: FullNodeTypes,
    Pool: TransactionPool,
{
    type Network = NoopNetwork;

    async fn build_network(
        self,
        ctx: &BuilderContext<N>,
        _pool: Pool,
    ) -> eyre::Result<Self::Network> {
        Ok(NoopNetwork::new().with_chain_id(ctx.chain_spec().chain_id()))
    }
}

/// Builds [`NoopConsensus`].
#[derive(Debug, Clone, Default)]
pub struct NoopConsensusBuilder;

impl<N> ConsensusBuilder<N> for NoopConsensusBuilder
where
    N: FullNodeTypes,
{
    type Consensus = NoopConsensus;

    async fn build_consensus(self, _ctx: &BuilderContext<N>) -> eyre::Result<Self::Consensus> {
        Ok(NoopConsensus::default())
    }
}

/// Builds [`PayloadBuilderHandle::noop`].
#[derive(Debug, Clone, Default)]
pub struct NoopPayloadBuilder;

impl<N, Pool> PayloadServiceBuilder<N, Pool> for NoopPayloadBuilder
where
    N: FullNodeTypes,
    Pool: TransactionPool,
{
    async fn spawn_payload_builder_service(
        self,
        _ctx: &BuilderContext<N>,
        _pool: Pool,
        _evm_config: BaseEvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle> {
        Ok(PayloadBuilderHandle::noop())
    }
}
