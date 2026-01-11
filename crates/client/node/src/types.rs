//! Type aliases for the OP node builder.

use std::sync::Arc;

use reth::{
    api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter},
    builder::{Node, NodeBuilder, NodeBuilderWithComponents, WithLaunchContext},
    providers::providers::BlockchainProvider,
};
use reth_db::DatabaseEnv;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpNode;

type OpNodeTypes = FullNodeTypesAdapter<OpNode, Arc<DatabaseEnv>, OpProvider>;
type OpComponentsBuilder = <OpNode as Node<OpNodeTypes>>::ComponentsBuilder;
type OpAddOns = <OpNode as Node<OpNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>;

/// OP Builder is a [`WithLaunchContext`] reth node builder.
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpNodeTypes, OpComponentsBuilder, OpAddOns>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;
