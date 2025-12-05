use std::sync::Arc;

use base_reth_flashblocks::state::FlashblocksState;
use once_cell::sync::OnceCell;
use reth::{
    api::{FullNodeTypesAdapter, NodeTypesWithDBAdapter},
    builder::{Node, NodeBuilderWithComponents, WithLaunchContext},
    providers::providers::BlockchainProvider,
};
use reth_db::DatabaseEnv;
use reth_optimism_node::OpNode;

pub(crate) type OpProvider = BlockchainProvider<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>;
type OpNodeTypes = FullNodeTypesAdapter<OpNode, Arc<DatabaseEnv>, OpProvider>;
type OpComponentsBuilder = <OpNode as Node<OpNodeTypes>>::ComponentsBuilder;
type OpAddOns = <OpNode as Node<OpNodeTypes>>::AddOns;
pub(crate) type OpBuilder =
    WithLaunchContext<NodeBuilderWithComponents<OpNodeTypes, OpComponentsBuilder, OpAddOns>>;

pub(crate) type FlashblocksCell = Arc<OnceCell<Arc<FlashblocksState<OpProvider>>>>;
