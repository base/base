//! Concrete component construction shared by Base node launchers.

use std::{marker::PhantomData, sync::Arc};

use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::builder::BasePayloadTransactions;
use base_node_context::BaseNodeContext;
use reth_provider::providers::BlockchainProvider;

use crate::{
    BaseNetworkBuilder, BasePayloadBuilder, BasePayloadServiceBuilder, BasePoolBuilder,
    BuilderContext, ComponentBuilder,
};

/// Constructs Base components while allowing the payload service to vary.
#[derive(Debug)]
pub struct BaseComponentsBuilder<Node, Payload = BasePayloadBuilder> {
    /// Transaction pool configuration.
    pub pool_builder: BasePoolBuilder,
    /// Payload service configuration.
    pub payload_builder: BasePayloadServiceBuilder<Payload>,
    /// Network configuration.
    pub network_builder: BaseNetworkBuilder,
    /// Provider adapter used during launch.
    pub node: PhantomData<Node>,
}

impl<Node, Payload> BaseComponentsBuilder<Node, Payload> {
    /// Creates a builder for the selected Base services.
    pub const fn new(
        pool_builder: BasePoolBuilder,
        payload_builder: BasePayloadServiceBuilder<Payload>,
        network_builder: BaseNetworkBuilder,
    ) -> Self {
        Self { pool_builder, payload_builder, network_builder, node: PhantomData }
    }

    /// Selects the payload service without changing the other Base components.
    pub fn payload<P>(
        self,
        payload_builder: BasePayloadServiceBuilder<P>,
    ) -> BaseComponentsBuilder<Node, P> {
        BaseComponentsBuilder {
            pool_builder: self.pool_builder,
            payload_builder,
            network_builder: self.network_builder,
            node: PhantomData,
        }
    }
}

impl<Txs> BaseComponentsBuilder<reth_db::DatabaseEnv, BasePayloadBuilder<Txs>>
where
    Txs: BasePayloadTransactions<base_node_context::BaseNodePool<BlockchainProvider>>,
{
    /// Converts Base component construction into a single launch callback.
    pub fn into_builder(self) -> ComponentBuilder {
        ComponentBuilder { build: Box::new(move |ctx| Box::pin(self.build_components(ctx))) }
    }

    /// Constructs the Base pool, network, payload service, and consensus validator.
    pub async fn build_components(self, ctx: &BuilderContext) -> eyre::Result<BaseNodeContext> {
        let evm_config = BaseEvmConfig::new(ctx.chain_spec());
        let pool = self.pool_builder.build_pool(ctx, evm_config.clone()).await?;
        let network = self.network_builder.build_network(ctx, pool.clone()).await?;
        let payload_builder_handle = self
            .payload_builder
            .spawn_payload_builder_service(ctx, pool.clone(), evm_config.clone())
            .await?;
        let consensus = Arc::new(BaseBeaconConsensus::new(ctx.chain_spec()));
        Ok(BaseNodeContext {
            provider: ctx.provider().clone(),
            task_executor: ctx.task_executor().clone(),
            transaction_pool: pool,
            evm_config,
            network,
            payload_builder_handle,
            consensus,
        })
    }
}
