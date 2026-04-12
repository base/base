//! Optional admin JSON-RPC handler.
//!
//! Provides `POST /` JSON-RPC admin methods mirroring the Go `op-proposer` API:
//! - `admin_startProposer`   — start the driver loop
//! - `admin_stopProposer`    — stop the driver loop
//! - `admin_proposerRunning` — query whether the driver is running

use std::{net::SocketAddr, sync::Arc};

use eyre::Context;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    server::{Server, ServerHandle},
    types::ErrorObjectOwned,
};
use tracing::info;

use crate::driver::ProposerDriverControl;

#[rpc(server, namespace = "admin")]
pub trait ProposerAdminApi {
    /// Start the proving pipeline.
    #[method(name = "startProposer")]
    async fn start_proposer(&self) -> RpcResult<()>;

    /// Stop the proving pipeline.
    #[method(name = "stopProposer")]
    async fn stop_proposer(&self) -> RpcResult<()>;

    /// Returns whether the proving pipeline is currently running.
    #[method(name = "proposerRunning")]
    async fn proposer_running(&self) -> RpcResult<bool>;
}

/// Concrete implementation of [`ProposerAdminApiServer`] backed by a
/// [`ProposerDriverControl`] handle.
pub struct ProposerAdminApiServerImpl {
    driver: Arc<dyn ProposerDriverControl>,
}

impl std::fmt::Debug for ProposerAdminApiServerImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposerAdminApiServerImpl")
            .field("driver", &"<dyn ProposerDriverControl>")
            .finish()
    }
}

impl ProposerAdminApiServerImpl {
    fn driver_error(msg: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(-32000, msg, None::<()>)
    }
}

#[async_trait]
impl ProposerAdminApiServer for ProposerAdminApiServerImpl {
    async fn start_proposer(&self) -> RpcResult<()> {
        self.driver.start_proposer().await.map_err(Self::driver_error)
    }

    async fn stop_proposer(&self) -> RpcResult<()> {
        self.driver.stop_proposer().await.map_err(Self::driver_error)
    }

    async fn proposer_running(&self) -> RpcResult<bool> {
        Ok(self.driver.is_running())
    }
}

/// A running admin JSON-RPC HTTP server.
///
/// Holds the jsonrpsee [`ServerHandle`] for the server's lifetime.
/// Dropping this value stops the server from accepting new connections.
pub struct AdminServer {
    handle: ServerHandle,
    addr: SocketAddr,
}

impl std::fmt::Debug for AdminServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminServer").field("addr", &self.addr).finish()
    }
}

impl AdminServer {
    /// Bind and start the admin server on the given socket address.
    pub async fn spawn(
        addr: SocketAddr,
        driver: Arc<dyn ProposerDriverControl>,
    ) -> eyre::Result<Self> {
        let server =
            Server::builder().build(addr).await.wrap_err("failed to bind admin RPC server")?;
        let addr = server.local_addr().wrap_err("failed to get admin server local address")?;
        let module = ProposerAdminApiServerImpl { driver }.into_rpc();
        let handle = server.start(module);
        info!(addr = %addr, "admin RPC server listening");
        Ok(Self { handle, addr })
    }

    /// Returns the local address the server is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Initiate graceful shutdown and wait for all in-flight connections to
    /// close.
    pub async fn shutdown(self) {
        let _ = self.handle.stop();
        self.handle.stopped().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::driver::ProposerDriverControl;

    struct MockDriverControl {
        running: AtomicBool,
    }

    impl MockDriverControl {
        fn new() -> Self {
            Self { running: AtomicBool::new(false) }
        }
    }

    #[async_trait]
    impl ProposerDriverControl for MockDriverControl {
        async fn start_proposer(&self) -> Result<(), String> {
            if self.running.load(Ordering::SeqCst) {
                return Err("already running".into());
            }
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn stop_proposer(&self) -> Result<(), String> {
            if !self.running.load(Ordering::SeqCst) {
                return Err("not running".into());
            }
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn test_admin_start_stop() {
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let server =
            AdminServer::spawn("127.0.0.1:0".parse().unwrap(), Arc::clone(&driver)).await.unwrap();
        let addr = server.local_addr();
        let client = reqwest::Client::new();

        // Start the proposer.
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_startProposer",
                "id": 1
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].is_null());
        assert!(driver.is_running());

        // Check running status.
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_proposerRunning",
                "id": 2
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["result"], true);

        // Stop the proposer.
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_stopProposer",
                "id": 3
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].is_null());
        assert!(!driver.is_running());
    }

    #[tokio::test]
    async fn test_admin_unknown_method() {
        let driver: Arc<dyn ProposerDriverControl> = Arc::new(MockDriverControl::new());
        let server = AdminServer::spawn("127.0.0.1:0".parse().unwrap(), driver).await.unwrap();
        let addr = server.local_addr();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "admin_doesNotExist",
                "id": 1
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32601);
        assert!(body["error"]["message"].as_str().unwrap().contains("not found"));
    }
}
