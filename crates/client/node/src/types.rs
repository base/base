//! Type aliases for the OP node builder.

use std::sync::Arc;

use reth_db::DatabaseEnv;
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeBuilder, NodeBuilderWithComponents, NodeTypesWithDBAdapter,
    WithLaunchContext,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::providers::BlockchainProvider;

use crate::node::BaseNode;

/// Internal alias for the OP node type adapter.
pub(crate) type OpNodeTypes = FullNodeTypesAdapter<BaseNode, Arc<DatabaseEnv>, OpProvider>;
/// Internal alias for the OP node components builder.
pub(crate) type OpComponentsBuilder = <BaseNode as Node<OpNodeTypes>>::ComponentsBuilder;
/// Internal alias for the OP node add-ons.
pub(crate) type OpAddOns = <BaseNode as Node<OpNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, Arc<DatabaseEnv>>>;

/// OP Builder is a [`WithLaunchContext`] reth node builder.
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpNodeTypes, OpComponentsBuilder, OpAddOns>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;
