use std::time::Duration;

use alloy_primitives::{B256, Bytes};
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use base_proof_tee_nitro_enclave::{EnclaveRequest, EnclaveResponse, Frame};
use tokio_vsock::{VsockAddr, VsockStream};
use tracing::info;

use crate::NitroHostError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SIGNER_TIMEOUT: Duration = Duration::from_secs(10);

/// Vsock-backed proof transport for Nitro Enclaves.
///
/// Each method call opens a fresh vsock connection, sends a length-prefixed
/// [`EnclaveRequest`] frame, reads the [`EnclaveResponse`], and closes.
///
/// Frame format: `[4-byte big-endian length][bincode payload]`.
#[derive(Debug, Clone)]
pub struct VsockTransport {
    cid: u32,
    port: u32,
}

impl VsockTransport {
    /// Create a transport targeting the given vsock endpoint.
    pub const fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }

    async fn connect(&self) -> Result<VsockStream, NitroHostError> {
        let addr = VsockAddr::new(self.cid, self.port);
        tokio::time::timeout(CONNECT_TIMEOUT, VsockStream::connect(addr))
            .await
            .map_err(|_| NitroHostError::ConnectTimeout)?
            .map_err(NitroHostError::ConnectFailed)
    }

    /// Send preimages to the enclave and return the proof result.
    pub async fn prove(
        &self,
        preimages: Vec<(PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult, NitroHostError> {
        let preimage_count = preimages.len();
        let total_value_bytes: usize = preimages.iter().map(|(_, v)| v.len()).sum();
        info!(
            preimage_count = preimage_count,
            total_value_bytes = total_value_bytes,
            cid = self.cid,
            port = self.port,
            "sending prove request to enclave"
        );

        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::Prove(preimages))
            .await
            .map_err(NitroHostError::FrameWrite)?;

        let response: EnclaveResponse =
            Frame::read(&mut stream).await.map_err(NitroHostError::FrameRead)?;

        match response {
            EnclaveResponse::Prove(result) => Ok(*result),
            EnclaveResponse::Error(e) => Err(NitroHostError::EnclaveRemoteError(e)),
            EnclaveResponse::AttestedWithdrawal(_)
            | EnclaveResponse::SignerPublicKey(_)
            | EnclaveResponse::SignerAttestation(_) => {
                Err(NitroHostError::UnexpectedResponse { expected: "prove" })
            }
        }
    }

    /// Verify a withdrawal record and return the enclave's raw signature.
    pub async fn sign_attested_withdrawal(
        &self,
        auth_hash: B256,
        message_passer_storage_root: B256,
        storage_proof: Vec<Bytes>,
    ) -> Result<Vec<u8>, NitroHostError> {
        let mut stream = self.connect().await?;
        Frame::write(
            &mut stream,
            &EnclaveRequest::SignAttestedWithdrawal {
                auth_hash,
                message_passer_storage_root,
                storage_proof,
            },
        )
        .await
        .map_err(NitroHostError::FrameWrite)?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| NitroHostError::ResponseTimeout {
                    operation: "sign_attested_withdrawal",
                })?
                .map_err(NitroHostError::FrameRead)?;
        match response {
            EnclaveResponse::AttestedWithdrawal(signature) => Ok(signature),
            EnclaveResponse::Error(error) => Err(NitroHostError::EnclaveRemoteError(error)),
            EnclaveResponse::Prove(_)
            | EnclaveResponse::SignerPublicKey(_)
            | EnclaveResponse::SignerAttestation(_) => {
                Err(NitroHostError::UnexpectedResponse { expected: "sign_attested_withdrawal" })
            }
        }
    }

    /// Return the 65-byte uncompressed ECDSA public key of the enclave signer.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>, NitroHostError> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::SignerPublicKey)
            .await
            .map_err(NitroHostError::FrameWrite)?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| NitroHostError::ResponseTimeout { operation: "signer_public_key" })?
                .map_err(NitroHostError::FrameRead)?;

        match response {
            EnclaveResponse::SignerPublicKey(key) => {
                if key.len() != 65 || key[0] != 0x04 {
                    return Err(NitroHostError::InvalidSignerKey);
                }
                Ok(key)
            }
            EnclaveResponse::Error(e) => Err(NitroHostError::EnclaveRemoteError(e)),
            EnclaveResponse::Prove(_)
            | EnclaveResponse::AttestedWithdrawal(_)
            | EnclaveResponse::SignerAttestation(_) => {
                Err(NitroHostError::UnexpectedResponse { expected: "signer_public_key" })
            }
        }
    }

    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes) for the enclave signer.
    pub async fn signer_attestation(
        &self,
        user_data: Option<Vec<u8>>,
        nonce: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, NitroHostError> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::SignerAttestation { user_data, nonce })
            .await
            .map_err(NitroHostError::FrameWrite)?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| NitroHostError::ResponseTimeout { operation: "signer_attestation" })?
                .map_err(NitroHostError::FrameRead)?;

        match response {
            EnclaveResponse::SignerAttestation(doc) => Ok(doc),
            EnclaveResponse::Error(e) => Err(NitroHostError::EnclaveRemoteError(e)),
            EnclaveResponse::Prove(_)
            | EnclaveResponse::AttestedWithdrawal(_)
            | EnclaveResponse::SignerPublicKey(_) => {
                Err(NitroHostError::UnexpectedResponse { expected: "signer_attestation" })
            }
        }
    }
}
