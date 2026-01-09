//! Type aliases

use std::sync::Arc;

use base_reth_flashblocks::FlashblocksState;
use once_cell::sync::OnceCell;
use reth::{
    api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter},
    builder::{Node, NodeBuilderWithComponents, WithLaunchContext},
    providers::providers::BlockchainProvider,
};
use reth_db::DatabaseEnv;
use reth_optimism_node::OpNode;

type OpNodeTypes = FullNodeTypesAdapter<OpNode, Arc<DatabaseEnv>, OpProvider>;
type OpComponentsBuilder = <OpNode as Node<OpNodeTypes>>::ComponentsBuilder;
type OpAddOns = <OpNode as Node<OpNodeTypes>>::AddOns;

/// A [`BlockchainProvider`] instance.
pub type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>;

/// OP Builder is a [`WithLaunchContext`] reth node builder.
pub type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpNodeTypes, OpComponentsBuilder, OpAddOns>>;

/// The flashblocks cell holds the [`FlashblocksState`].
pub type FlashblocksCell = Arc<OnceCell<Arc<FlashblocksState<OpProvider>>>>;
