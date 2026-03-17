use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

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

/// Maximum bytes per chunk when writing a frame over vsock.
///
/// Large payloads (tens/hundreds of megabytes) sent as a single `write_all`
/// can be corrupted by the vsock transport layer. Splitting into chunks with
/// per-chunk CRC32 checksums lets us detect corruption early.
const CHUNK_SIZE: usize = 256 * 1024;

/// Maximum bytes per individual `write()` syscall on vsock.
///
/// Linux kernel commit `6693731487a8` (Aug 2025) changed `virtio_vsock` to
/// allocate nonlinear SKBs (scattered across multiple pages) for packets
/// larger than `PAGE_ALLOC_COSTLY_ORDER` (typically 32 KiB on x86). The
/// hypervisor-side virtio handler may not correctly reassemble these
/// multi-descriptor TX packets, causing silent data corruption.
///
/// By capping each `write()` to 32 KiB we force the kernel to use simple,
/// linear (single-page) SKB allocations, sidestepping the bug entirely.
///
/// See: <https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7672>
const MAX_WRITE_SIZE: usize = 32 * 1024;

/// Length-prefixed, chunked bincode codec with per-chunk CRC32 integrity.
///
/// Wire format:
/// ```text
/// [4B total_len][4B chunk_count]
///   [4B chunk_len][4B crc32][chunk bytes]   // chunk 0
///   [4B chunk_len][4B crc32][chunk bytes]   // chunk 1
///   ...
///   [4B chunk_len][4B crc32][chunk bytes]   // chunk N-1
/// ```
///
/// Both sides must use this format — it is **not** backward-compatible with
/// the previous `[4B len][payload]` format.
#[derive(Debug, Clone, Copy)]
pub struct Frame;

/// Write `data` in sub-chunks of at most [`MAX_WRITE_SIZE`] bytes.
///
/// This prevents the kernel from allocating nonlinear SKBs for vsock
/// transmissions. See [`MAX_WRITE_SIZE`] for details on the underlying bug.
async fn write_throttled(
    writer: &mut (impl AsyncWriteExt + Unpin),
    data: &[u8],
) -> TransportResult<()> {
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + MAX_WRITE_SIZE).min(data.len());
        writer.write_all(&data[offset..end]).await?;
        offset = end;
    }
    Ok(())
}

impl Frame {
    /// Write a value as a chunked, CRC32-checked bincode frame.
    pub async fn write<T: serde::Serialize>(
        writer: &mut (impl AsyncWriteExt + Unpin),
        value: &T,
    ) -> TransportResult<()> {
        let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        let total_len = u32::try_from(payload.len())
            .map_err(|_| TransportError::Codec("payload exceeds u32::MAX".into()))?;

        let chunks: Vec<&[u8]> = payload.chunks(CHUNK_SIZE).collect();
        let chunk_count = chunks.len() as u32;

        info!(
            payload_bytes = payload.len(),
            chunk_count = chunk_count,
            chunk_size = CHUNK_SIZE,
            "frame write start"
        );

        writer.write_u32(total_len).await?;
        writer.write_u32(chunk_count).await?;

        for (i, chunk) in chunks.iter().enumerate() {
            let crc = crc32fast::hash(chunk);

            writer.write_u32(chunk.len() as u32).await?;
            writer.write_u32(crc).await?;
            write_throttled(&mut writer, chunk).await?;
            writer.flush().await?;

            if i % 10 == 0 || i == chunks.len() - 1 {
                info!(
                    chunk_index = i,
                    chunk_bytes = chunk.len(),
                    crc32 = format_args!("{crc:#010x}"),
                    "chunk written"
                );
            }

        }

        info!(payload_bytes = payload.len(), "frame write complete");
        Ok(())
    }

    /// Read a value from a chunked, CRC32-checked bincode frame.
    ///
    /// The theoretical maximum frame size is `u32::MAX` (~4 GiB). All transport
    /// peers run locally within the same host (enclave ↔ host over vsock), and
    /// witness bundles can be large, so we intentionally allow the full u32
    /// range rather than imposing an artificial cap.
    pub async fn read<T: serde::de::DeserializeOwned>(
        reader: &mut (impl AsyncReadExt + Unpin),
    ) -> TransportResult<T> {
        let total_len = reader.read_u32().await? as usize;
        let chunk_count = reader.read_u32().await? as usize;

        info!(
            payload_bytes = total_len,
            chunk_count = chunk_count,
            "frame read start"
        );

        let mut payload = Vec::with_capacity(total_len);

        for i in 0..chunk_count {
            let chunk_len = reader.read_u32().await? as usize;
            let expected_crc = reader.read_u32().await?;

            let mut chunk = vec![0u8; chunk_len];
            reader.read_exact(&mut chunk).await?;

            let actual_crc = crc32fast::hash(&chunk);

            if actual_crc != expected_crc {
                return Err(TransportError::Codec(format!(
                    "chunk {i}/{chunk_count} crc32 mismatch: \
                     expected {expected_crc:#010x}, got {actual_crc:#010x} \
                     (chunk_len={chunk_len}, total_len={total_len})"
                )));
            }

            payload.extend_from_slice(&chunk);

            if i % 10 == 0 || i == chunk_count - 1 {
                info!(
                    chunk_index = i,
                    chunk_bytes = chunk_len,
                    crc32 = format_args!("{actual_crc:#010x}"),
                    "chunk verified"
                );
            }
        }

        if payload.len() != total_len {
            return Err(TransportError::Codec(format!(
                "reassembled payload length mismatch: header={total_len}, actual={}",
                payload.len()
            )));
        }

        let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        info!(payload_bytes = total_len, "frame read complete");
        Ok(value)
    }
}
