#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "linux")]
use tokio::time::{Duration, timeout};
#[cfg(target_os = "linux")]
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
#[cfg(target_os = "linux")]
use tracing::{debug, info, warn};

#[cfg(target_os = "linux")]
use crate::transport::Frame;

mod attestation;
pub use attestation::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, VerificationResult,
    get_default_ca_root, verify_attestation,
};

mod crypto;
pub use crypto::{Ecdsa, Signing};

mod nsm;
pub use nsm::{NsmRng, NsmSession};

mod protocol;
pub use protocol::{EnclaveRequest, EnclaveResponse};

mod server;
pub use server::Server;

/// Fixed vsock port the enclave listens on.
pub const VSOCK_PORT: u32 = 8000;

/// Deadline for receiving a complete request frame from the host.
///
/// Must be generous enough to cover large [`EnclaveRequest::Prove`] payloads,
/// which include the full preimage bundle and can be many megabytes over vsock.
/// A single timeout applies to all request types because the request type is
/// unknown until the frame has been fully read.
#[cfg(target_os = "linux")]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Nitro Enclave runtime.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct NitroEnclave {
    server: Arc<Server>,
}

#[cfg(target_os = "linux")]
impl NitroEnclave {
    /// Create a new enclave runtime.
    pub fn new() -> eyre::Result<Self> {
        let server = Arc::new(Server::new()?);
        info!(address = %server.signer_address(), "enclave initialized");
        Ok(Self { server })
    }

    /// Listen on vsock, prove blocks, return results.
    pub async fn run(self) -> eyre::Result<()> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, VSOCK_PORT))?;
        info!(cid = VMADDR_CID_ANY, port = VSOCK_PORT, "listening on vsock");

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(cid = peer.cid(), port = peer.port(), "accepted connection");

            let enclave = self.clone();
            tokio::spawn(async move {
                if let Err(e) = enclave.handle_connection(stream).await {
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

    /// Handle a single vsock connection: read request, dispatch, write response.
    async fn handle_connection(&self, mut stream: tokio_vsock::VsockStream) -> eyre::Result<()> {
        let request: EnclaveRequest =
            timeout(REQUEST_READ_TIMEOUT, Frame::read(&mut stream)).await??;

        match request {
            EnclaveRequest::Prove(preimages) => {
                let preimage_count = preimages.len();
                let total_value_bytes: usize = preimages.iter().map(|(_, v)| v.len()).sum();
                info!(
                    preimage_count = preimage_count,
                    total_value_bytes = total_value_bytes,
                    "received prove request"
                );
                let response = match self.server.prove(preimages).await {
                    Ok(result) => EnclaveResponse::Prove(Box::new(result)),
                    Err(e) => EnclaveResponse::Error(e.to_string()),
                };
                Frame::write(&mut stream, &response).await?;
            }
            EnclaveRequest::SignerPublicKey => {
                info!("received signer public key request");
                let key = self.server.signer_public_key();
                Frame::write(&mut stream, &EnclaveResponse::SignerPublicKey(key)).await?;
            }
            EnclaveRequest::SignerAttestation => {
                info!("received signer attestation request");
                // nsm_init() and nsm_process_request() are blocking FFI calls; use
                // block_in_place so they do not stall the async executor.
                let result = tokio::task::block_in_place(|| self.server.signer_attestation());
                let response = match result {
                    Ok(doc) => EnclaveResponse::SignerAttestation(doc),
                    Err(e) => EnclaveResponse::Error(e.to_string()),
                };
                Frame::write(&mut stream, &response).await?;
            }
        }

        Ok(())
    }
}
