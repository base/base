//! Type aliases for the OP node builder.

use std::sync::Arc;

use reth::{
    api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter},
    builder::{Node, NodeBuilder, NodeBuilderWithComponents, WithLaunchContext},
    providers::providers::BlockchainProvider,
};
use reth_db::DatabaseEnv;
use reth_optimism_chainspec::OpChainSpec;
use crate::node::BaseNode;

type OpNodeTypes = FullNodeTypesAdapter<BaseNode, Arc<DatabaseEnv>, OpProvider>;
type OpComponentsBuilder = <BaseNode as Node<OpNodeTypes>>::ComponentsBuilder;
type OpAddOns = <BaseNode as Node<OpNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<BaseNode, Arc<DatabaseEnv>>>;

/// OP Builder is a [`WithLaunchContext`] reth node builder.
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpNodeTypes, OpComponentsBuilder, OpAddOns>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;
