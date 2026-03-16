use std::{io, time::Duration};

use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use tokio_vsock::{VsockAddr, VsockStream};

use crate::{
    NitroError,
    enclave::{EnclaveRequest, EnclaveResponse},
    transport::Frame,
};

const PROVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
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

    async fn connect(&self) -> Result<VsockStream, NitroError> {
        let addr = VsockAddr::new(self.cid, self.port);
        tokio::time::timeout(CONNECT_TIMEOUT, VsockStream::connect(addr))
            .await
            .map_err(|_| {
                NitroError::Transport(
                    io::Error::new(io::ErrorKind::TimedOut, "connect timed out").to_string(),
                )
            })?
            .map_err(|e| NitroError::Transport(e.to_string()))
    }

    /// Send preimages to the enclave and return the proof result.
    pub async fn prove(
        &self,
        preimages: Vec<(PreimageKey, Vec<u8>)>,
    ) -> Result<ProofResult, NitroError> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::Prove(preimages))
            .await
            .map_err(|e| NitroError::Transport(e.to_string()))?;

        let response: EnclaveResponse =
            tokio::time::timeout(PROVE_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| {
                    NitroError::Transport(
                        io::Error::new(io::ErrorKind::TimedOut, "prove timed out").to_string(),
                    )
                })?
                .map_err(|e| NitroError::Transport(e.to_string()))?;

        match response {
            EnclaveResponse::Prove(result) => Ok(*result),
            EnclaveResponse::Error(e) => Err(NitroError::Transport(e)),
            EnclaveResponse::SignerPublicKey(_) | EnclaveResponse::SignerAttestation(_) => {
                Err(NitroError::Transport("unexpected response type for prove".into()))
            }
        }
    }

    /// Return the 65-byte uncompressed ECDSA public key of the enclave signer.
    pub async fn signer_public_key(&self) -> Result<Vec<u8>, NitroError> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::SignerPublicKey)
            .await
            .map_err(|e| NitroError::Transport(e.to_string()))?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| {
                    NitroError::Transport(
                        io::Error::new(io::ErrorKind::TimedOut, "signer_public_key timed out")
                            .to_string(),
                    )
                })?
                .map_err(|e| NitroError::Transport(e.to_string()))?;

        match response {
            EnclaveResponse::SignerPublicKey(key) => {
                if key.len() != 65 || key[0] != 0x04 {
                    return Err(NitroError::Transport(
                        "invalid signer public key: expected 65-byte uncompressed SEC1 key".into(),
                    ));
                }
                Ok(key)
            }
            EnclaveResponse::Error(e) => Err(NitroError::Transport(e)),
            EnclaveResponse::Prove(_) | EnclaveResponse::SignerAttestation(_) => {
                Err(NitroError::Transport("unexpected response type for signer_public_key".into()))
            }
        }
    }

    /// Return the raw Nitro attestation document (`COSE_Sign1` bytes) for the enclave signer.
    pub async fn signer_attestation(&self) -> Result<Vec<u8>, NitroError> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::SignerAttestation)
            .await
            .map_err(|e| NitroError::Transport(e.to_string()))?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream))
                .await
                .map_err(|_| {
                    NitroError::Transport(
                        io::Error::new(io::ErrorKind::TimedOut, "signer_attestation timed out")
                            .to_string(),
                    )
                })?
                .map_err(|e| NitroError::Transport(e.to_string()))?;

        match response {
            EnclaveResponse::SignerAttestation(doc) => Ok(doc),
            EnclaveResponse::Error(e) => Err(NitroError::Transport(e)),
            EnclaveResponse::Prove(_) | EnclaveResponse::SignerPublicKey(_) => {
                Err(NitroError::Transport("unexpected response type for signer_attestation".into()))
            }
        }
    }
}
