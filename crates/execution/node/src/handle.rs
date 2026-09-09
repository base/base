use std::fmt;

use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_node_core::exit::NodeExitFuture;

use crate::{full_node::FullNode, rpc::RethRpcAddOns};

/// A Handle to the launched node.
#[must_use = "Needs to await the node exit future"]
pub struct NodeHandle<
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    AddOns: RethRpcAddOns<DB>,
> {
    /// All node components.
    pub node: FullNode<DB, AddOns>,
    /// The exit future of the node.
    pub node_exit_future: NodeExitFuture,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, AddOns> NodeHandle<DB, AddOns>
where
    AddOns: RethRpcAddOns<DB>,
{
    /// Waits for the node to exit, if it was configured to exit.
    pub async fn wait_for_node_exit(self) -> eyre::Result<()> {
        self.node_exit_future.await
    }
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, AddOns> fmt::Debug
    for NodeHandle<DB, AddOns>
where
    AddOns: RethRpcAddOns<DB>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHandle")
            .field("node", &"...")
            .field("node_exit_future", &self.node_exit_future)
            .finish()
    }
}
