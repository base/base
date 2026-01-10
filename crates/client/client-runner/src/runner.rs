//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use base_client_primitives::{BaseNodeExtension, ConfigurableBaseNodeExtension};
use eyre::Result;
use reth::{
    builder::{EngineNodeLauncher, Node, NodeHandleFor, TreeConfig},
    providers::providers::BlockchainProvider,
};
use reth_optimism_node::OpNode;
use tracing::info;

use crate::{BaseNodeBuilder, BaseNodeConfig, BaseNodeHandle};

/// Wraps the Base node configuration and orchestrates builder wiring.
#[derive(Debug)]
pub struct BaseNodeRunner {
    /// Contains the configuration for the Base node.
    config: BaseNodeConfig,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
}

impl BaseNodeRunner {
    /// Creates a new launcher using the provided configuration.
    pub fn new(config: impl Into<BaseNodeConfig>) -> Self {
        Self { config: config.into(), extensions: Vec::new() }
    }

    /// Returns the underlying configuration, primarily for testing.
    pub const fn config(&self) -> &BaseNodeConfig {
        &self.config
    }

    /// Registers a new builder extension constructed from the node configuration.
    pub fn install_ext<E>(&mut self) -> Result<()>
    where
        E: ConfigurableBaseNodeExtension<BaseNodeConfig>,
    {
        let extension = E::build(&self.config)?;
        self.extensions.push(Box::new(extension));
        Ok(())
    }

    /// Applies all Base-specific wiring to the supplied builder, launches the node, and returns a handle that can be awaited.
    pub fn run(self, builder: BaseNodeBuilder) -> BaseNodeHandle {
        let Self { config, extensions } = self;
        BaseNodeHandle::new(Self::launch_node(config, extensions, builder))
    }

    async fn launch_node(
        config: BaseNodeConfig,
        extensions: Vec<Box<dyn BaseNodeExtension>>,
        builder: BaseNodeBuilder,
    ) -> Result<NodeHandleFor<OpNode>> {
        info!(target: "base-runner", "starting custom Base node");

        let op_node = OpNode::new(config.rollup_args.clone());

        let builder = builder
            .with_types_and_provider::<OpNode, BlockchainProvider<_>>()
            .with_components(op_node.components())
            .with_add_ons(op_node.add_ons())
            .on_component_initialized(move |_ctx| Ok(()));

        let builder =
            extensions.into_iter().fold(builder, |builder, extension| extension.apply(builder));

        builder
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
            .await
    }
}
