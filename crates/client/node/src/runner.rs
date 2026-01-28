//! Contains the [`BaseNodeRunner`], which is responsible for configuring and launching a Base node.

use eyre::Result;
use reth_node_builder::{EngineNodeLauncher, Node, NodeHandleFor, TreeConfig};
use reth_optimism_node::args::RollupArgs;
use reth_provider::providers::BlockchainProvider;
use tracing::info;

use crate::{
    BaseBuilder, BaseNodeBuilder, BaseNodeExtension, BaseNodeHandle, FromExtensionConfig,
    node::BaseNode,
};

/// The threshold for the number of blocks in the WAL before emitting a warning.
///
/// Base has ~2 second block times, which is 6x faster than Ethereum mainnet (~12 seconds).
/// The default threshold of 128 blocks would trigger warnings too frequently on Base,
/// so we use 6x the default (768) to maintain similar warning frequency.
const BASE_WAL_BLOCKS_WARNING: usize = 768;

/// Wraps the Base node configuration and orchestrates builder wiring.
#[derive(Debug)]
pub struct BaseNodeRunner {
    /// Rollup-specific arguments forwarded to the Optimism node implementation.
    rollup_args: RollupArgs,
    /// Registered builder extensions.
    extensions: Vec<Box<dyn BaseNodeExtension>>,
}

impl BaseNodeRunner {
    /// Creates a new launcher using the provided rollup arguments.
    pub fn new(rollup_args: RollupArgs) -> Self {
        Self { rollup_args, extensions: Vec::new() }
    }

    /// Registers a new builder extension.
    pub fn install_ext<T: FromExtensionConfig + 'static>(&mut self, config: T::Config) {
        self.extensions.push(Box::new(T::from_config(config)));
    }

    /// Applies all Base-specific wiring to the supplied builder, launches the node, and returns a
    /// handle that can be awaited.
    pub fn run(self, builder: BaseNodeBuilder) -> BaseNodeHandle {
        let Self { rollup_args, extensions } = self;
        BaseNodeHandle::new(Self::launch_node(rollup_args, extensions, builder))
    }

    async fn launch_node(
        rollup_args: RollupArgs,
        extensions: Vec<Box<dyn BaseNodeExtension>>,
        builder: BaseNodeBuilder,
    ) -> Result<NodeHandleFor<BaseNode>> {
        info!(target: "base-runner", "starting custom Base node");

        let base_node = BaseNode::new(rollup_args);

        let builder = builder
            .with_types_and_provider::<BaseNode, BlockchainProvider<_>>()
            .with_components(base_node.components())
            .with_add_ons(base_node.add_ons())
            .with_wal_blocks_warning(BASE_WAL_BLOCKS_WARNING)
            .on_component_initialized(move |_ctx| Ok(()));

        let builder = extensions
            .into_iter()
            .fold(BaseBuilder::new(builder), |builder, extension| extension.apply(builder));

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
