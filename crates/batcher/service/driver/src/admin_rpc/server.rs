//! Admin JSON-RPC HTTP server lifecycle.

use std::net::SocketAddr;

use base_batcher_core::AdminHandle;
use eyre::Context;
use jsonrpsee::server::{Server, ServerHandle};
use tracing::info;

use crate::{BatcherAdminApiServer, BatcherAdminApiServerImpl};

/// A running admin JSON-RPC HTTP server.
///
/// Holds the jsonrpsee [`ServerHandle`] for the server's lifetime.
/// Dropping this value stops the server from accepting new connections.
#[derive(derive_more::Debug)]
pub struct AdminServer {
    #[debug(skip)]
    handle: ServerHandle,
}

impl AdminServer {
    /// Bind and start the admin server on the given socket address.
    pub async fn spawn(addr: SocketAddr, admin_handle: AdminHandle) -> eyre::Result<Self> {
        let server =
            Server::builder().build(addr).await.wrap_err("failed to bind admin RPC server")?;
        let addr = server.local_addr().wrap_err("failed to get admin server local address")?;
        let module = BatcherAdminApiServerImpl::new(admin_handle).into_rpc();
        let handle = server.start(module);
        info!(addr = %addr, "admin RPC server listening");
        Ok(Self { handle })
    }

    /// Future that resolves when the server stops.
    pub async fn stopped(&self) {
        self.handle.clone().stopped().await;
    }
}
