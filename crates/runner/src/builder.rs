use std::sync::Arc;

use eyre::Result;
use once_cell::sync::OnceCell;
use reth::{
    builder::{EngineNodeLauncher, Node, NodeHandle, TreeConfig},
    providers::providers::BlockchainProvider,
};
use reth_optimism_node::OpNode;
use tracing::info;

use crate::{
    BaseNodeBuilder, BaseNodeConfig, BaseRpcExtension, FlashblocksCanonExtension,
    TransactionTracingExtension,
};

/// Wraps the Base node configuration and orchestrates builder wiring.
#[derive(Debug, Clone)]
pub struct BaseNodeLauncher {
    config: BaseNodeConfig,
}

impl BaseNodeLauncher {
    /// Creates a new launcher using the provided configuration.
    pub fn new(config: impl Into<BaseNodeConfig>) -> Self {
        Self { config: config.into() }
    }

    /// Returns the underlying configuration, primarily for testing.
    pub const fn config(&self) -> &BaseNodeConfig {
        &self.config
    }

    /// Applies all Base-specific wiring to the supplied builder and launches the node.
    pub async fn build_and_run(&self, builder: BaseNodeBuilder) -> Result<()> {
        info!(target: "base-runner", "starting custom Base node");

        let op_node = OpNode::new(self.config.rollup_args.clone());

        let flashblocks_cell = Arc::new(OnceCell::new());
        let flashblocks_config = self.config.flashblocks.clone();

        let builder = builder
            .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
            .with_components(op_node.components())
            .with_add_ons(op_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        // Apply the Flashblocks Canon extension
        let flashblocks_extension =
            FlashblocksCanonExtension::new(flashblocks_cell.clone(), flashblocks_config.clone());
        let builder = flashblocks_extension.apply(builder);

        // Apply the Transaction Tracing extension
        let tracing_extension = TransactionTracingExtension::new(self.config.tracing);
        let builder = tracing_extension.apply(builder);

        let rpc_extension = BaseRpcExtension::new(
            flashblocks_cell.clone(),
            flashblocks_config.clone(),
            self.config.metering_enabled,
            self.config.rollup_args.sequencer.clone(),
        );
        let builder = rpc_extension.apply(builder);

        let NodeHandle { node: _, node_exit_future } = builder
            .launch_with_fn(|builder| {
                let engine_tree_config = TreeConfig::default()
                    .with_persistence_threshold(builder.config().engine.persistence_threshold)
                    .with_memory_block_buffer_target(
                        builder.config().engine.memory_block_buffer_target,
                    );

                let launcher = EngineNodeLauncher::new(
                    builder.task_executor().clone(),
                    builder.config().datadir(),
                    engine_tree_config,
                );

                builder.launch_with(launcher)
            })
            .await?;

        node_exit_future.await
    }
}
