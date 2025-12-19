//! Traits describing configurable node builder extensions.

use std::fmt::Debug;

use eyre::Result;
use reth::builder::NodeHandleFor;
use reth_optimism_node::OpNode;

use crate::{BaseNodeConfig, extensions::OpBuilder};

/// A node builder extension that can apply additional wiring to the builder.
pub trait BaseNodeExtension: Send + Sync + Debug {
    /// Applies the extension to the supplied builder.
    fn apply(&self, builder: OpBuilder) -> OpBuilder;

    /// Called after the node is launched with access to the full node handle.
    ///
    /// This is useful for extensions that need to register P2P subprotocols,
    /// spawn background tasks, or access the network layer.
    ///
    /// Default implementation does nothing.
    fn on_node_started(&self, _node: &NodeHandleFor<OpNode>) -> Result<()> {
        Ok(())
    }
}

/// An extension that can be constructed from [`BaseNodeConfig`].
pub trait ConfigurableBaseNodeExtension: BaseNodeExtension + Sized + 'static {
    /// Builds the extension from the node config.
    fn build(config: &BaseNodeConfig) -> Result<Self>;
}
