use std::fmt;

use reth_node_core::exit::NodeExitFuture;

use crate::{full_node::FullNode, rpc::RethRpcAddOns};

/// A Handle to the launched node.
#[must_use = "Needs to await the node exit future"]
pub struct NodeHandle<AddOns: RethRpcAddOns> {
    /// All node components.
    pub node: FullNode<AddOns>,
    /// The exit future of the node.
    pub node_exit_future: NodeExitFuture,
}

impl<AddOns> NodeHandle<AddOns>
where
    AddOns: RethRpcAddOns,
{
    /// Waits for the node to exit, if it was configured to exit.
    pub async fn wait_for_node_exit(self) -> eyre::Result<()> {
        self.node_exit_future.await
    }
}

impl<AddOns> fmt::Debug for NodeHandle<AddOns>
where
    AddOns: RethRpcAddOns,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHandle")
            .field("node", &"...")
            .field("node_exit_future", &self.node_exit_future)
            .finish()
    }
}
