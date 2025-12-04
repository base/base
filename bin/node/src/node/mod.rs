//! Contains the node building and running logic

use std::sync::Arc;

use reth::builder::{NodeBuilder, WithLaunchContext};
use reth_db::DatabaseEnv;
use reth_optimism_chainspec::OpChainSpec;

mod builder;
pub use builder::BaseNodeLauncher;

mod config;
pub use config::{BaseNodeConfig, FlashblocksConfig, TracingConfig};

type BaseNodeBuilder = WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, OpChainSpec>>;
