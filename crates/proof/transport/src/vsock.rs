use std::time::Duration;

use async_trait::async_trait;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use tokio_vsock::{VsockAddr, VsockStream};

use crate::{
    EnclaveRequest, EnclaveResponse, Frame, ProofTransport, TransportError, TransportResult,
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

    async fn connect(&self) -> TransportResult<VsockStream> {
        let addr = VsockAddr::new(self.cid, self.port);
        tokio::time::timeout(CONNECT_TIMEOUT, VsockStream::connect(addr))
            .await
            .map_err(|_| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timed out",
                ))
            })?
            .map_err(TransportError::Io)
    }
}

#[async_trait]
impl ProofTransport for VsockTransport {
    async fn prove(&self, preimages: &[(PreimageKey, Vec<u8>)]) -> TransportResult<ProofResult> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::Prove(preimages.to_vec())).await?;

        let response: EnclaveResponse = tokio::time::timeout(
            PROVE_TIMEOUT,
            Frame::read(&mut stream),
        )
        .await
        .map_err(|_| {
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "prove timed out"))
        })??;

        match response {
            EnclaveResponse::Prove(result) => Ok(*result),
            EnclaveResponse::Error(e) => Err(TransportError::Enclave(e)),
            _ => Err(TransportError::Codec("unexpected response type for prove".into())),
        }
    }

    async fn signer_public_key(&self) -> TransportResult<Vec<u8>> {
        let mut stream = self.connect().await?;

        Frame::write(&mut stream, &EnclaveRequest::SignerPublicKey).await?;

        let response: EnclaveResponse =
            tokio::time::timeout(SIGNER_TIMEOUT, Frame::read(&mut stream)).await.map_err(
                |_| {
                    TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "signer_public_key timed out",
                    ))
                },
            )??;

        match response {
            EnclaveResponse::SignerPublicKey(key) => Ok(key),
            EnclaveResponse::Error(e) => Err(TransportError::Enclave(e)),
            _ => {
                Err(TransportError::Codec("unexpected response type for signer_public_key".into()))
            }
        }
    }
}
