/// Enclave runtime: vsock listener, server, crypto, NSM, and attestation.
use alloy_primitives::{Address, B256};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use base_proof_primitives::{ProofBundle, ProofResult};
#[cfg(target_os = "linux")]
use base_proof_transport::Frame;
#[cfg(target_os = "linux")]
use tracing::{debug, info};
#[cfg(target_os = "linux")]
use vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

mod attestation;
pub use attestation::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, VerificationResult,
    VerifyOptions, extract_public_key, get_default_ca_root, verify_attestation,
    verify_attestation_with_options, verify_attestation_with_pcr0,
    verify_attestation_with_pcr0_and_options,
};

mod crypto;
pub use crypto::{Ecdsa, SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, Signing};

mod nsm;
pub use nsm::{NsmRng, NsmSession};

mod server;
pub use server::Server;

/// Enclave runtime configuration.
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
pub struct NitroEnclave {
    server: Server,
    vsock_port: u32,
}

#[cfg(target_os = "linux")]
impl NitroEnclave {
    /// Create a new enclave runtime from the given configuration.
    pub fn new(config: &EnclaveConfig) -> eyre::Result<Self> {
        let server = Server::new(config)?;
        info!(address = %server.signer_address(), "enclave initialized");
        Ok(Self { server, vsock_port: config.vsock_port })
    }

    /// Listen on vsock, prove blocks, return results.
    pub async fn run(self) -> eyre::Result<()> {
        const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

        let listener = VsockListener::bind(&VsockAddr::new(VMADDR_CID_ANY, self.vsock_port))?;
        info!(port = self.vsock_port, "listening on vsock");

        loop {
            let (mut stream, peer) = listener.accept()?;
            debug!(cid = peer.cid(), port = peer.port(), "accepted connection");

            stream.set_read_timeout(Some(READ_TIMEOUT))?;
            let bundle: ProofBundle = Frame::read(&mut stream)?;
            let result: ProofResult = self.server.prove(bundle).await?;
            Frame::write(&mut stream, &result)?;
        }
    }
}
