use std::sync::Arc;

#[cfg(target_os = "linux")]
use super::vsock::VsockTransport;
use crate::{NitroError, enclave::Server};
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;

/// Nitro prover transport.
///
/// Abstracts over vsock (production, Linux-only) and in-process (local dev) modes.
#[derive(Debug)]
pub enum NitroTransport {
    /// Send proving requests to a Nitro Enclave over vsock.
    #[cfg(target_os = "linux")]
    Vsock(VsockTransport),
    /// Run the enclave server in-process (no vsock required).
    Local(Arc<Server>),
}

impl NitroTransport {
    /// Create a vsock transport targeting the given enclave endpoint.
    #[cfg(target_os = "linux")]
    pub fn vsock(cid: u32, port: u32) -> Self {
        Self::Vsock(VsockTransport::new(cid, port))
    }

    /// Create a local in-process transport backed by the given enclave server.
    pub const fn local(server: Arc<Server>) -> Self {
        Self::Local(server)
    }

    /// Send preimages to the prover and return the proof result.
    pub async fn prove(
        &self,
        preimages: &[(PreimageKey, Vec<u8>)],
    ) -> Result<ProofResult, NitroError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.prove(preimages).await,
            Self::Local(s) => s.prove(preimages.to_vec()).await,
        }
    }

    /// Return the 65-byte uncompressed ECDSA public key of the enclave signer.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>, NitroError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.signer_public_key().await,
            Self::Local(s) => Ok(s.signer_public_key()),
        }
    }
}
