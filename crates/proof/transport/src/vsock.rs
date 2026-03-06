use std::{
    io::{Read, Write},
    time::Duration,
};

use async_trait::async_trait;
use base_proof_primitives::{ProofBundle, ProofResult};
use vsock::{VsockAddr, VsockStream};

use crate::{ProofTransport, TransportError, TransportResult};

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

fn write_frame<T: serde::Serialize>(writer: &mut impl Write, value: &T) -> TransportResult<()> {
    let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| TransportError::Codec(e.to_string()))?;

    let len = u32::try_from(payload.len())
        .map_err(|_| TransportError::Codec("payload exceeds u32::MAX".into()))?;

    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> TransportResult<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;

    let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
        .map_err(|e| TransportError::Codec(e.to_string()))?;

    Ok(value)
}

#[async_trait]
impl ProofTransport for VsockTransport {
    async fn prove(&self, bundle: &ProofBundle) -> TransportResult<ProofResult> {
        let bundle = bundle.clone();
        let addr = VsockAddr::new(self.cid, self.port);
        let task = tokio::task::spawn_blocking(move || {
            let mut stream = VsockStream::connect(&addr)?;
            write_frame(&mut stream, &bundle)?;
            read_frame(&mut stream)
        });
        tokio::time::timeout(PROVE_TIMEOUT, task).await.map_err(|_| {
            TransportError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "prove timed out"))
        })??
    }
}
