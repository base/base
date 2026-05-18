//! Type aliases for the Base node builder.

use base_execution_chainspec::BaseChainSpec;
use reth_db::DatabaseEnv;
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeBuilder, NodeTypesWithDBAdapter, WithLaunchContext,
};
use reth_provider::providers::BlockchainProvider;

use crate::node::BaseNode;

/// Alias for the Base node type adapter used by the runner.
pub type BaseNodeTypes = FullNodeTypesAdapter<BaseNode, DatabaseEnv, BaseProvider>;
/// Internal alias for the Base node components builder (default payload service).
pub type BaseComponentsBuilder = <BaseNode as Node<BaseNodeTypes>>::ComponentsBuilder;
/// Internal alias for the Base node add-ons (all generics resolved).
pub(crate) type ConcreteBaseAddOns = <BaseNode as Node<BaseNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type BaseProvider = BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, DatabaseEnv>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<DatabaseEnv, BaseChainSpec>>;
