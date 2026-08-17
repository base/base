//! A variant over the channel [`CompressorWriter`] implementations.

use crate::{
    BrotliCompressor, CompressionAlgo, CompressorResult, CompressorWriter, ZlibCompressor,
};

/// Dispatches [`CompressorWriter`] operations to Brotli or zlib.
#[derive(Debug, Clone)]
pub enum VariantCompressor {
    /// The brotli compressor.
    Brotli(BrotliCompressor),
    /// The zlib compressor.
    Zlib(ZlibCompressor),
}

impl From<CompressionAlgo> for VariantCompressor {
    fn from(algo: CompressionAlgo) -> Self {
        match algo {
            lvl @ (CompressionAlgo::Brotli9
            | CompressionAlgo::Brotli10
            | CompressionAlgo::Brotli11) => Self::Brotli(BrotliCompressor::new(lvl)),
            CompressionAlgo::Zlib => Self::Zlib(ZlibCompressor::new()),
        }
    }
}

impl CompressorWriter for VariantCompressor {
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
        match self {
            Self::Brotli(c) => c.write(data),
            Self::Zlib(c) => c.write(data),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Brotli(c) => c.reset(),
            Self::Zlib(c) => c.reset(),
        }
    }

    fn compressed_len(&self) -> CompressorResult<usize> {
        match self {
            Self::Brotli(c) => c.compressed_len(),
            Self::Zlib(c) => c.compressed_len(),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        match self {
            Self::Brotli(c) => c.read(buf),
            Self::Zlib(c) => c.read(buf),
        }
    }

    fn channel_version_byte(&self) -> Option<u8> {
        match self {
            Self::Brotli(c) => c.channel_version_byte(),
            Self::Zlib(c) => c.channel_version_byte(),
        }
    }
}
