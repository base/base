//! Test Utilities for the compression crate.

use alloc::vec::Vec;

use alloy_primitives::Bytes;

use crate::{ChannelCompressor, CompressorError, CompressorResult, CompressorWriter};

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

    fn flush(&mut self) -> CompressorResult<()> {
        Ok(())
    }

    fn close(&mut self) -> CompressorResult<()> {
        Ok(())
    }

    fn reset(&mut self) {
        self.compressed = None;
    }

    fn len(&self) -> usize {
        self.compressed.as_ref().map(|b: &Bytes| b.len()).unwrap_or(0)
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        if self.read_error {
            return Err(CompressorError::Full);
        }
        let len = self.compressed.as_ref().map(|b: &Bytes| b.len()).unwrap_or(0);
        buf[..len].copy_from_slice(
            self.compressed.as_ref().expect("mock compressed output must be configured"),
        );
        Ok(len)
    }
}

impl ChannelCompressor for MockCompressor {
    fn get_compressed(&self) -> Vec<u8> {
        self.compressed.as_ref().expect("mock compressed output must be configured").to_vec()
    }

    fn channel_version_byte(&self) -> Option<u8> {
        self.version_byte
    }
}
