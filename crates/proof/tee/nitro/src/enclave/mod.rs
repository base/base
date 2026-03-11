#[cfg(target_os = "linux")]
use std::sync::Arc;

use alloy_primitives::{Address, B256};
#[cfg(target_os = "linux")]
use base_proof_preimage::PreimageKey;
#[cfg(target_os = "linux")]
use base_proof_primitives::ProofResult;
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
pub use crypto::{Ecdsa, SIGNING_DATA_BASE_LENGTH, Signing};

mod nsm;
pub use nsm::{NsmRng, NsmSession};

mod server;
pub use server::Server;

/// Enclave runtime configuration.
#[derive(Debug)]
pub struct EnclaveConfig {
    /// Vsock port to listen on.
    pub vsock_port: u32,
    /// The proposer address (set at deploy time).
    pub proposer: Address,
    /// Per-chain configuration hash.
    pub config_hash: B256,
    /// Expected PCR0 measurement. Verified against NSM at startup.
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

#[cfg(target_os = "linux")]
async fn handle_connection(
    mut stream: tokio_vsock::VsockStream,
    server: &Server,
) -> eyre::Result<()> {
    const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    let preimages: Vec<(PreimageKey, Vec<u8>)> =
        timeout(READ_TIMEOUT, Frame::read(&mut stream)).await??;
    let result: ProofResult = server.prove(preimages).await?;
    Frame::write(&mut stream, &result).await?;
    Ok(())
}
