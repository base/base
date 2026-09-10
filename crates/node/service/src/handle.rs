use std::fmt;

use base_node_config::NodeExitFuture;

use crate::FullNode;

/// A Handle to the launched node.
#[must_use = "Needs to await the node exit future"]
pub struct NodeHandle {
    /// All node components.
    pub node: FullNode,
    /// The exit future of the node.
    pub node_exit_future: NodeExitFuture,
}

impl NodeHandle {
    /// Waits for the node to exit, if it was configured to exit.
    pub async fn wait_for_node_exit(self) -> eyre::Result<()> {
        self.node_exit_future.await
    }
}

impl fmt::Debug for NodeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHandle")
            .field("node", &"...")
            .field("node_exit_future", &self.node_exit_future)
            .finish()
    }
}
