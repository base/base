//! Type aliases for the OP node builder.

use reth_db::DatabaseEnv;
use reth_node_builder::{
    FullNodeTypesAdapter, Node, NodeBuilder, NodeTypesWithDBAdapter, WithLaunchContext,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::providers::BlockchainProvider;

use crate::node::BaseNode;

/// Alias for the OP node type adapter used by the runner.
pub type OpNodeTypes = FullNodeTypesAdapter<BaseNode, DatabaseEnv, OpProvider>;
/// Internal alias for the OP node components builder (default payload service).
pub(crate) type OpComponentsBuilder = <BaseNode as Node<OpNodeTypes>>::ComponentsBuilder;
/// Internal alias for the OP node add-ons.
pub(crate) type OpAddOns = <BaseNode as Node<OpNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, DatabaseEnv>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<DatabaseEnv, OpChainSpec>>;
