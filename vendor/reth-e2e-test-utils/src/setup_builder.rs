//! Builder for configuring and creating test node setups.
//!
//! This module provides a flexible builder API for setting up test nodes with custom
//! configurations through closures that modify `NodeConfig` and `TreeConfig`.

use std::{fmt::Debug, sync::Arc};

use base_common_chain_config::BaseChainSpec;
use base_common_runtime_tasks::Runtime;
use base_execution_payload_types::BasePayloadBuilderAttributes;
use base_node_config::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use base_node_core::{NodeConfig, NodeHandle};
use futures_util::future::TryJoinAll;
use reth_primitives_traits::AlloyBlockHeader;
use tracing::{Instrument, Level, span};

use crate::{NodeHelperType, node::NodeTestContext, wallet::Wallet};

/// Type alias for tree config modifier closure
type TreeConfigModifier = Box<
    dyn Fn(base_execution_engine_types::TreeConfig) -> base_execution_engine_types::TreeConfig
        + Send
        + Sync,
>;

/// Type alias for node config modifier closure
type NodeConfigModifier = Box<dyn Fn(NodeConfig) -> NodeConfig + Send + Sync>;

/// Builder for configuring and creating test node setups.
///
/// This builder allows customizing test node configurations through closures that
/// modify `NodeConfig` and `TreeConfig`. It avoids code duplication by centralizing
/// the node creation logic.
pub struct E2ETestSetupBuilder<F>
where
    F: Fn(u64) -> BasePayloadBuilderAttributes + Send + Sync + Copy + 'static,
{
    num_nodes: usize,
    chain_spec: Arc<BaseChainSpec>,
    attributes_generator: F,
    connect_nodes: bool,
    tree_config_modifier: Option<TreeConfigModifier>,
    node_config_modifier: Option<NodeConfigModifier>,
}

impl<F> E2ETestSetupBuilder<F>
where
    F: Fn(u64) -> BasePayloadBuilderAttributes + Send + Sync + Copy + 'static,
{
    /// Creates a new builder with the required parameters.
    pub fn new(num_nodes: usize, chain_spec: Arc<BaseChainSpec>, attributes_generator: F) -> Self {
        Self {
            num_nodes,
            chain_spec,
            attributes_generator,
            connect_nodes: true,
            tree_config_modifier: None,
            node_config_modifier: None,
        }
    }

    /// Sets whether nodes should be interconnected (default: true).
    pub const fn with_connect_nodes(mut self, connect_nodes: bool) -> Self {
        self.connect_nodes = connect_nodes;
        self
    }

    /// Sets a modifier function for the tree configuration.
    ///
    /// The closure receives the base tree config and returns a modified version.
    pub fn with_tree_config_modifier<G>(mut self, modifier: G) -> Self
    where
        G: Fn(base_execution_engine_types::TreeConfig) -> base_execution_engine_types::TreeConfig
            + Send
            + Sync
            + 'static,
    {
        self.tree_config_modifier = Some(Box::new(modifier));
        self
    }

    /// Sets a modifier function for the node configuration.
    ///
    /// The closure receives the base node config and returns a modified version.
    pub fn with_node_config_modifier<G>(mut self, modifier: G) -> Self
    where
        G: Fn(NodeConfig) -> NodeConfig + Send + Sync + 'static,
    {
        self.node_config_modifier = Some(Box::new(modifier));
        self
    }

    /// Sets the pruning arguments for the test nodes.
    pub fn with_pruning(self, pruning: base_node_config::PruningArgs) -> Self {
        self.with_node_config_modifier(move |config| config.with_pruning(pruning.clone()))
    }

    /// Builds and launches the test nodes.
    pub async fn build(
        self,
        node_factory: impl Fn() -> base_node_core::BaseNode + Send + Sync,
    ) -> eyre::Result<(Vec<NodeHelperType>, Wallet)> {
        let runtime = Runtime::test();

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs { disable_discovery: true, ..DiscoveryArgs::default() },
            ..NetworkArgs::default()
        };

        // Apply tree config modifier if present, with test-appropriate defaults
        let base_tree_config = base_execution_engine_types::TreeConfig::default()
            .with_cross_block_cache_size(1024 * 1024);
        let tree_config = if let Some(modifier) = self.tree_config_modifier {
            modifier(base_tree_config)
        } else {
            base_tree_config
        };

        let mut nodes = (0..self.num_nodes)
            .map(async |idx| {
                // Create base node config
                let base_config = NodeConfig::new(self.chain_spec.clone())
                    .with_network(network_config.clone())
                    .with_unused_ports()
                    .with_rpc(RpcServerArgs::default().with_unused_ports().with_http());

                // Apply node config modifier if present
                let node_config = if let Some(modifier) = &self.node_config_modifier {
                    modifier(base_config)
                } else {
                    base_config
                };

                let span = span!(Level::INFO, "node", idx);
                let mut launch = base_node_core::NodeLaunch::testing(node_config, runtime.clone());
                launch.base = node_factory();
                launch.engine_tree_config = tree_config.clone();
                let NodeHandle { node, node_exit_future: _ } =
                    launch.launch().instrument(span).await?;

                let node = NodeTestContext::new(node, self.attributes_generator).await?;
                let genesis_number = self.chain_spec.genesis_header().number();
                let genesis = node.block_hash(genesis_number);
                node.update_forkchoice(genesis, genesis).await?;

                eyre::Ok(node)
            })
            .collect::<TryJoinAll<_>>()
            .await?;

        for idx in 0..self.num_nodes {
            let (prev, current) = nodes.split_at_mut(idx);
            let current = current.first_mut().unwrap();
            // Connect nodes if requested
            if self.connect_nodes {
                if let Some(prev_idx) = idx.checked_sub(1) {
                    prev[prev_idx].connect(current).await;
                }

                // Connect last node with the first if there are more than two
                if idx + 1 == self.num_nodes
                    && self.num_nodes > 2
                    && let Some(first) = prev.first_mut()
                {
                    current.connect(first).await;
                }
            }
        }

        Ok((nodes, Wallet::default().with_chain_id(self.chain_spec.chain().into())))
    }
}

impl<F> Debug for E2ETestSetupBuilder<F>
where
    F: Fn(u64) -> BasePayloadBuilderAttributes + Send + Sync + Copy + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2ETestSetupBuilder")
            .field("num_nodes", &self.num_nodes)
            .field("connect_nodes", &self.connect_nodes)
            .field("tree_config_modifier", &self.tree_config_modifier.as_ref().map(|_| "<closure>"))
            .field("node_config_modifier", &self.node_config_modifier.as_ref().map(|_| "<closure>"))
            .finish_non_exhaustive()
    }
}
