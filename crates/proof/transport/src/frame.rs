use std::io::{Read, Write};

use crate::{TransportError, TransportResult};

/// Length-prefixed bincode codec.
///
/// Frame format: `[4-byte big-endian length][bincode payload]`.
#[derive(Debug, Clone, Copy)]
pub struct Frame;

impl Frame {
    /// Write a value as a length-prefixed bincode frame.
    pub fn write<T: serde::Serialize>(writer: &mut impl Write, value: &T) -> TransportResult<()> {
        let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        let len = u32::try_from(payload.len())
            .map_err(|_| TransportError::Codec("payload exceeds u32::MAX".into()))?;

        writer.write_all(&len.to_be_bytes())?;
        writer.write_all(&payload)?;
        writer.flush()?;
        Ok(())
    }

    /// Read a value from a length-prefixed bincode frame.
    pub fn read<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> TransportResult<T> {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;

        let (value, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .map_err(|e| TransportError::Codec(e.to_string()))?;

        Ok(value)
    }
}
