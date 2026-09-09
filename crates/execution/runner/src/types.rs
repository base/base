//! Type aliases for the Base node builder.

use base_node_core::{NodeHandle, NodeLaunch};
use reth_db::DatabaseEnv;
use reth_provider::providers::BlockchainProvider;

/// Alias for the Base node type adapter used by the runner.
pub type BaseNodeTypes = DatabaseEnv;
/// Concrete components produced by the Base node runner.
pub type BaseNodeComponents = base_node_context::BaseNodeContext;

/// Handle returned by the Base node launcher.
pub type BaseNodeHandle = NodeHandle;

/// A [`BlockchainProvider`] instance.
pub type BaseProvider = BlockchainProvider;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = NodeLaunch;
