//! Sync status RPC API trait.

use base_protocol::SyncStatus;
#[cfg(feature = "rpc-server")]
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// Base sync status RPC API.
#[cfg_attr(
    all(feature = "rpc-server", not(feature = "rpc-client")),
    rpc(server, namespace = "optimism")
)]
#[cfg_attr(
    all(not(feature = "rpc-server"), feature = "rpc-client"),
    rpc(client, namespace = "optimism")
)]
#[cfg_attr(
    all(feature = "rpc-server", feature = "rpc-client"),
    rpc(server, client, namespace = "optimism")
)]
pub trait SyncStatusApi {
    /// Returns the current [`SyncStatus`] of the node.
    #[method(name = "syncStatus")]
    async fn sync_status(&self) -> RpcResult<SyncStatus>;
}

#[cfg(all(test, feature = "rpc-server"))]
mod tests {
    use async_trait::async_trait;
    use base_protocol::SyncStatus;
    use jsonrpsee::core::RpcResult;
    use rstest::rstest;

    use super::SyncStatusApiServer;

    struct StubSyncStatusApi;

    #[async_trait]
    impl SyncStatusApiServer for StubSyncStatusApi {
        async fn sync_status(&self) -> RpcResult<SyncStatus> {
            unimplemented!()
        }
    }

    #[rstest]
    #[case("optimism_syncStatus")]
    fn sync_status_api_wire_names(#[case] expected: &str) {
        let module = StubSyncStatusApi.into_rpc();
        let names: Vec<&str> = module.method_names().collect();
        assert!(names.contains(&expected), "missing method {expected}, got: {names:?}");
    }
}
