use std::time::Duration;

use async_trait::async_trait;
use base_proof_primitives::{ProofBundle, ProofResult};
use vsock::{VsockAddr, VsockStream};

use crate::{Frame, ProofTransport, TransportError, TransportResult};

const PROVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
    async fn prove(&self, bundle: &ProofBundle) -> TransportResult<ProofResult> {
        let bundle = bundle.clone();
        let addr = VsockAddr::new(self.cid, self.port);
        let task = tokio::task::spawn_blocking(move || {
            let mut stream = VsockStream::connect(&addr)?;
            stream.set_read_timeout(Some(PROVE_TIMEOUT - Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            Frame::write(&mut stream, &bundle)?;
            Frame::read(&mut stream)
        });
        tokio::time::timeout(PROVE_TIMEOUT, task).await.map_err(|_| {
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "prove timed out"))
        })??
    }
}
