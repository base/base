use std::time::Duration;

use async_trait::async_trait;
use base_proof_preimage::PreimageKey;
use base_proof_primitives::ProofResult;
use tokio_vsock::{VsockAddr, VsockStream};

use crate::{Frame, ProofTransport, TransportError, TransportResult};

const PROVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Vsock-backed proof transport for Nitro Enclaves.
///
/// Each [`prove`](ProofTransport::prove) call opens a fresh vsock connection,
/// sends a length-prefixed bincode frame, reads the response, and closes.
///
/// Frame format: `[4-byte big-endian length][bincode payload]`.
#[derive(Debug, Clone)]
pub struct VsockTransport {
    cid: u32,
    port: u32,
}

impl VsockTransport {
    /// Create a transport targeting the given vsock endpoint.
    pub fn new(cid: u32, port: u32) -> Self {
        Self { cid, port }
    }
}

#[async_trait]
impl ProofTransport for VsockTransport {
    async fn prove(
        &self,
        preimages: &[(PreimageKey, Vec<u8>)],
    ) -> TransportResult<ProofResult> {
        let addr = VsockAddr::new(self.cid, self.port);

        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, VsockStream::connect(addr))
            .await
            .map_err(|_| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timed out",
            ))
        })??;

        Frame::write(&mut stream, &preimages).await?;

        tokio::time::timeout(PROVE_TIMEOUT, Frame::read(&mut stream)).await.map_err(|_| {
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "prove timed out"))
        })?
    }
}
