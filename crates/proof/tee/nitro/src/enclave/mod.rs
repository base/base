#[cfg(target_os = "linux")]
use std::sync::Arc;

use alloy_primitives::B256;
#[cfg(target_os = "linux")]
use base_proof_transport::Frame;
#[cfg(target_os = "linux")]
use tokio::time::{Duration, timeout};
#[cfg(target_os = "linux")]
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
#[cfg(target_os = "linux")]
use tracing::{debug, info, warn};

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

/// Enclave runtime configuration.
#[derive(Debug)]
pub struct EnclaveConfig {
    /// Vsock port to listen on.
    pub vsock_port: u32,
    /// Per-chain configuration hash.
    pub config_hash: B256,
    /// Expected TEE image hash. In enclave mode, verified as keccak256(PCR0) against NSM at startup.
    pub tee_image_hash: B256,
}

/// Nitro Enclave runtime.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct NitroEnclave {
    server: Arc<Server>,
    vsock_port: u32,
}

#[cfg(target_os = "linux")]
impl NitroEnclave {
    /// Create a new enclave runtime from the given configuration.
    pub fn new(config: &EnclaveConfig) -> eyre::Result<Self> {
        let server = Arc::new(Server::new(config)?);
        info!(address = %server.signer_address(), "enclave initialized");
        Ok(Self { server, vsock_port: config.vsock_port })
    }

    /// Listen on vsock, prove blocks, return results.
    pub async fn run(self) -> eyre::Result<()> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, self.vsock_port))?;
        info!(port = self.vsock_port, "listening on vsock");

        loop {
            let (stream, peer) = listener.accept().await?;
            debug!(cid = peer.cid(), port = peer.port(), "accepted connection");

            let server = Arc::clone(&self.server);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, &server).await {
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

/// Deadline for receiving a complete request frame from the host.
///
/// Must be generous enough to cover large [`EnclaveRequest::Prove`] payloads,
/// which include the full preimage bundle and can be many megabytes over vsock.
#[cfg(target_os = "linux")]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[cfg(target_os = "linux")]
async fn handle_connection(
    mut stream: tokio_vsock::VsockStream,
    server: &Server,
) -> eyre::Result<()> {
    let request: EnclaveRequest = timeout(REQUEST_READ_TIMEOUT, Frame::read(&mut stream)).await??;

    match request {
        EnclaveRequest::Prove(preimages) => {
            let response = match server.prove(preimages).await {
                Ok(result) => EnclaveResponse::Prove(Box::new(result)),
                Err(e) => EnclaveResponse::Error(e.to_string()),
            };
            Frame::write(&mut stream, &response).await?;
        }
        EnclaveRequest::SignerPublicKey => {
            let key = server.signer_public_key();
            Frame::write(&mut stream, &EnclaveResponse::SignerPublicKey(key)).await?;
        }
        EnclaveRequest::SignerAttestation => {
            let response = match server.signer_attestation() {
                Ok(doc) => EnclaveResponse::SignerAttestation(doc),
                Err(e) => EnclaveResponse::Error(e.to_string()),
            };
            Frame::write(&mut stream, &response).await?;
        }
    }

    Ok(())
}
