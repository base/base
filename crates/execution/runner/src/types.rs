//! Type aliases for the Base node builder.

use base_execution_rpc::eth::BaseEthApiBuilder;
use reth_db::DatabaseEnv;
use reth_node_builder::{
    FullNodeTypesAdapter, NodeBuilder, NodeHandle, NodeTypesWithDBAdapter, WithLaunchContext,
};
use reth_provider::providers::BlockchainProvider;

use crate::{BaseAddOns, BaseNodeAdapter};

/// Alias for the Base node type adapter used by the runner.
pub type BaseNodeTypes = FullNodeTypesAdapter<DatabaseEnv, BaseProvider>;
/// Concrete components produced by the Base node runner.
pub type BaseNodeComponents = base_node_core::BaseNodeComponents<BaseNodeTypes>;
/// Concrete RPC and engine add-ons for the Base runner.
pub type BaseNodeAddOns = BaseAddOns<BaseNodeAdapter, BaseEthApiBuilder>;

/// Handle returned by the Base node launcher.
pub type BaseNodeHandle = NodeHandle<BaseNodeAdapter, BaseNodeAddOns>;

/// A [`BlockchainProvider`] instance.
pub type BaseProvider = BlockchainProvider<NodeTypesWithDBAdapter<DatabaseEnv>>;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<DatabaseEnv>>;
