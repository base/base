//! Enclave runtime and vsock listener.

#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
#[cfg(target_os = "linux")]
use tracing::{debug, info, warn};

#[cfg(target_os = "linux")]
use crate::{EnclaveHandler, Server};

/// Fixed vsock port the enclave listens on.
pub const VSOCK_PORT: u32 = 8000;

/// Nitro Enclave runtime.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct NitroEnclave {
    handler: EnclaveHandler,
}

#[cfg(target_os = "linux")]
impl NitroEnclave {
    /// Create a new enclave runtime.
    pub fn new() -> eyre::Result<Self> {
        let server = Arc::new(Server::new()?);
        info!(address = %server.signer_address(), "enclave initialized");
        Ok(Self { handler: EnclaveHandler::new(server) })
    }

    /// Listen on vsock, prove blocks, return results.
    pub async fn run(self) -> eyre::Result<()> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, VSOCK_PORT))?;
        info!(cid = VMADDR_CID_ANY, port = VSOCK_PORT, "listening on vsock");

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(cid = peer.cid(), port = peer.port(), "accepted connection");

            let handler = self.handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handler.serve(stream).await {
                    warn!(
                        error = %e,
                        cid = peer.cid(),
                        port = peer.port(),
                        "connection failed"
                    );
                }
            });
        }
    }
}
