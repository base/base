use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{TransportError, TransportResult};

/// Length-prefixed bincode codec.
///
/// Frame format: `[4-byte big-endian length][bincode payload]`.
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

        writer.write_u32(len).await?;
        writer.write_all(&payload).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Read a value from a length-prefixed bincode frame.
    ///
    /// The theoretical maximum frame size is `u32::MAX` (~4 `GiB`). All transport
    /// peers run locally within the same host (enclave ↔ host over vsock), and
    /// witness bundles can be large, so we intentionally allow the full u32
    /// range rather than imposing an artificial cap.
    pub async fn read<T: serde::de::DeserializeOwned>(
        reader: &mut (impl AsyncReadExt + Unpin),
    ) -> TransportResult<T> {
        let len = reader.read_u32().await? as usize;

        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;

        let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        Ok(value)
    }
}
