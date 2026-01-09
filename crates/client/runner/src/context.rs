//! Contains a type alias for the base node builder used in the runner.

use std::sync::Arc;

use reth::builder::{NodeBuilder, WithLaunchContext};
use reth_db::DatabaseEnv;
use reth_optimism_chainspec::OpChainSpec;

/// Convenience alias for the Base node builder type.
pub type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;
