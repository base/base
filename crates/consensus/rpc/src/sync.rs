use base_protocol::SyncStatus;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// Optimism sync status RPC API.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "optimism"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "optimism"))]
pub trait SyncStatusApi {
    /// Returns the current [`SyncStatus`] of the node.
    #[method(name = "syncStatus")]
    async fn op_sync_status(&self) -> RpcResult<SyncStatus>;
}
