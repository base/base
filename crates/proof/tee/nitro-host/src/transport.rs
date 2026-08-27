use std::sync::Arc;

use alloy_primitives::{Address, B256};
use alloy_signer::utils::public_key_to_address;
#[cfg(feature = "metrics")]
use base_proof_host::Metrics;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use base_proof_tee_nitro_enclave::Server;
use k256::ecdsa::VerifyingKey;

#[cfg(target_os = "linux")]
use super::vsock::VsockTransport;
use crate::NitroHostError;

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
    pub const fn vsock(cid: u32, port: u32) -> Self {
        Self::Vsock(VsockTransport::new(cid, port))
    }

    /// Create a local in-process transport backed by the given enclave server.
    pub const fn local(server: Arc<Server>) -> Self {
        Self::Local(server)
    }

    /// Send preimages to the prover and return the proof result.
    pub async fn prove(
        &self,
        preimages: Vec<(PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult, NitroHostError> {
        #[cfg(feature = "metrics")]
        Metrics::witness_size_bytes(Metrics::PROVER_NITRO)
            .record(preimages.iter().map(|(_, value)| value.len()).sum::<usize>() as f64);

        Ok(match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.prove(preimages).await?,
            Self::Local(s) => Box::pin(s.prove(preimages)).await?,
        })
    }

    /// Return the 65-byte uncompressed ECDSA public key of the enclave signer.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>, NitroHostError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.signer_public_key().await,
            Self::Local(s) => Ok(s.signer_public_key()),
        }
    }

    /// Return the enclave image hash derived from PCR0.
    pub async fn tee_image_hash(&self) -> Result<B256, NitroHostError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.tee_image_hash().await,
            Self::Local(s) => Ok(s.tee_image_hash()),
        }
    }

    /// Return the Ethereum address of the enclave signer.
    pub async fn signer_address(&self) -> Result<Address, NitroHostError> {
        let key = self.signer_public_key().await?;
        let key =
            VerifyingKey::from_sec1_bytes(&key).map_err(|_| NitroHostError::InvalidSignerKey)?;
        Ok(public_key_to_address(&key))
    }

    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes) for the enclave signer.
    pub async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, NitroHostError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(t) => t.signer_attestation(user_data, nonce).await,
            Self::Local(s) => s.signer_attestation(user_data, nonce).map_err(Into::into),
        }
    }
}
