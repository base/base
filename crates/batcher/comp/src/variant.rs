//! A variant over the different implementations of [`ChannelCompressor`].

use alloc::vec::Vec;

use base_consensus_genesis::RollupConfig;

use crate::{
    BrotliCompressor, ChannelCompressor, CompressionAlgo, CompressorResult, CompressorWriter,
    ZlibCompressor,
};

/// The channel compressor wraps the brotli and zlib compressor types,
/// implementing the [`ChannelCompressor`] trait itself.
#[derive(Debug, Clone)]
pub enum VariantCompressor {
    /// The brotli compressor.
    Brotli(BrotliCompressor),
    /// The zlib compressor.
    Zlib(ZlibCompressor),
}

impl VariantCompressor {
    /// Constructs a [`VariantCompressor`] using the given [`RollupConfig`] and timestamp.
    pub fn from_timestamp(config: &RollupConfig, timestamp: u64) -> Self {
        if config.is_fjord_active(timestamp) {
            Self::Brotli(BrotliCompressor::new(CompressionAlgo::Brotli10))
        } else {
            Self::Zlib(ZlibCompressor::new())
        }
    }
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

    fn flush(&mut self) -> CompressorResult<()> {
        match self {
            Self::Brotli(c) => c.flush(),
            Self::Zlib(c) => c.flush(),
        }
    }

    fn close(&mut self) -> CompressorResult<()> {
        match self {
            Self::Brotli(c) => c.close(),
            Self::Zlib(c) => c.close(),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Brotli(c) => c.reset(),
            Self::Zlib(c) => c.reset(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Brotli(c) => c.len(),
            Self::Zlib(c) => c.len(),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        match self {
            Self::Brotli(c) => c.read(buf),
            Self::Zlib(c) => c.read(buf),
        }
    }
}

impl ChannelCompressor for VariantCompressor {
    fn get_compressed(&self) -> Vec<u8> {
        match self {
            Self::Brotli(c) => c.get_compressed(),
            Self::Zlib(c) => c.get_compressed(),
        }
    }

    fn channel_version_byte(&self) -> Option<u8> {
        match self {
            Self::Brotli(c) => c.channel_version_byte(),
            Self::Zlib(c) => c.channel_version_byte(),
        }
    }
}
