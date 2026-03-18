use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

/// Result type for proof transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors that can occur during proof transport operations.
#[derive(Error, Debug)]
pub enum TransportError {
    /// An I/O error occurred on the underlying stream.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization or deserialization of a message failed.
    #[error("codec error: {0}")]
    Codec(String),
}

/// Maximum bytes per individual `write()` syscall on vsock.
///
/// Linux kernel commit `6693731487a8` (Aug 2025) changed `virtio_vsock` to
/// allocate nonlinear SKBs (scattered across multiple pages) for packets
/// larger than `PAGE_ALLOC_COSTLY_ORDER` (typically 32 `KiB` on x86). The
/// hypervisor-side virtio handler may not correctly reassemble these
/// multi-descriptor TX packets, causing silent data corruption.
///
/// By capping each `write()` to 28 `KiB` we force the kernel to use simple,
/// linear (single-page) SKB allocations, sidestepping the bug entirely.
///
/// See: <https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7672>
/// 28 `KiB` — comfortably below the ~32384-byte linear SKB threshold
const MAX_WRITE_SIZE: usize = 28 * 1024;

/// Length-prefixed bincode codec over `AsyncRead`/`AsyncWrite`.
///
/// Wire format: `[4B big-endian length][bincode payload]`
///
/// Writes are throttled to [`MAX_WRITE_SIZE`]-byte segments to avoid
/// triggering a Linux kernel vsock corruption bug.
#[derive(Debug, Clone, Copy)]
pub struct Frame;

impl Frame {
    /// Write a value as a length-prefixed bincode frame.
    pub async fn write<T: serde::Serialize>(
        writer: &mut (impl AsyncWriteExt + Unpin),
        value: &T,
    ) -> TransportResult<()> {
        let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        let len = u32::try_from(payload.len())
            .map_err(|_| TransportError::Codec("payload exceeds u32::MAX".into()))?;

        debug!(payload_bytes = payload.len(), "frame write start");

        writer.write_u32(len).await?;
        Self::write_throttled(writer, &payload).await?;
        writer.flush().await?;

        debug!(payload_bytes = payload.len(), "frame write complete");
        Ok(())
    }

    /// Read a value from a length-prefixed bincode frame.
    ///
    /// The peer-supplied length can be up to `u32::MAX` (~4 `GiB`). This is safe
    /// because all transport peers are local (enclave ↔ host over vsock) and
    /// witness bundles can legitimately be very large.
    pub async fn read<T: serde::de::DeserializeOwned>(
        reader: &mut (impl AsyncReadExt + Unpin),
    ) -> TransportResult<T> {
        let len = reader.read_u32().await? as usize;

        debug!(payload_bytes = len, "frame read start");

        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;

        let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        debug!(payload_bytes = len, "frame read complete");
        Ok(value)
    }

    async fn write_throttled(
        writer: &mut (impl AsyncWriteExt + Unpin),
        data: &[u8],
    ) -> TransportResult<()> {
        for chunk in data.chunks(MAX_WRITE_SIZE) {
            writer.write_all(chunk).await?;
        }
        Ok(())
    }
}
