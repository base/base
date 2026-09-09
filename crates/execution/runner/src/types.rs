//! Type aliases for the Base node builder.

use base_node_core::{NodeBuilder, NodeHandle, WithLaunchContext};
use reth_db::DatabaseEnv;
use reth_provider::providers::BlockchainProvider;

use crate::BaseAddOns;

/// Alias for the Base node type adapter used by the runner.
pub type BaseNodeTypes = DatabaseEnv;
/// Concrete components produced by the Base node runner.
pub type BaseNodeComponents = base_node_context::BaseNodeContext;
/// Concrete RPC and engine add-ons for the Base runner.
pub type BaseNodeAddOns = BaseAddOns;

/// Handle returned by the Base node launcher.
pub type BaseNodeHandle = NodeHandle<BaseNodeAddOns>;

/// A [`BlockchainProvider`] instance.
pub type BaseProvider = BlockchainProvider;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<DatabaseEnv>>;
