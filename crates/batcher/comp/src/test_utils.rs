//! Test Utilities for the compression crate.

use alloy_primitives::Bytes;

use crate::{CompressorError, CompressorResult, CompressorWriter};

/// A Mock compressor for testing.
#[derive(Debug, Clone, Default)]
pub struct MockCompressor {
    /// Compressed bytes
    pub compressed: Option<Bytes>,
    /// Whether to throw a read error.
    pub read_error: bool,
    /// Optional channel version prefix.
    pub version_byte: Option<u8>,
}

impl CompressorWriter for MockCompressor {
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
        let data = data.to_vec();
        let written = data.len();
        self.compressed = Some(Bytes::from(data));
        Ok(written)
    }

    fn reset(&mut self) {
        self.compressed = None;
    }

    fn compressed_len(&self) -> CompressorResult<usize> {
        Ok(self.compressed.as_ref().map(|b: &Bytes| b.len()).unwrap_or(0))
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        if self.read_error {
            return Err(CompressorError::Full);
        }
        let compressed = self.compressed.take().unwrap_or_default();
        let len = compressed.len().min(buf.len());
        buf[..len].copy_from_slice(&compressed[..len]);
        if len < compressed.len() {
            self.compressed = Some(compressed.slice(len..));
        }
        Ok(len)
    }

    fn channel_version_byte(&self) -> Option<u8> {
        self.version_byte
    }
}
