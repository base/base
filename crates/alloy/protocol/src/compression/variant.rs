//! A variant over the different implementations of [Compressor].

use crate::{BrotliCompressor, CompressionAlgo, Compressor, ZlibCompressor};
use alloc::vec::Vec;
use op_alloy_genesis::RollupConfig;

/// The channel compressor wraps the brotli and zlib compressor types,
/// implementing the [Compressor] trait itself.
#[derive(Debug, Clone)]
pub enum VariantCompressor {
    /// The brotli compressor.
    Brotli(BrotliCompressor),
    /// The zlib compressor.
    Zlib(ZlibCompressor),
}

impl VariantCompressor {
    /// Constructs a [VariantCompressor] using the given [RollupConfig] and timestamp.
    pub fn from_timestamp(config: &RollupConfig, timestamp: u64) -> Self {
        if config.is_fjord_active(timestamp) {
            Self::Brotli(BrotliCompressor::new(CompressionAlgo::Brotli10))
        } else {
            Self::Zlib(ZlibCompressor)
        }
    }
}

impl Compressor for VariantCompressor {
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Brotli(compressor) => compressor.compress(data),
            Self::Zlib(compressor) => compressor.compress(data),
        }
    }
}

impl From<CompressionAlgo> for VariantCompressor {
    fn from(algo: CompressionAlgo) -> Self {
        match algo {
            lvl @ CompressionAlgo::Brotli9 => Self::Brotli(BrotliCompressor::new(lvl)),
            lvl @ CompressionAlgo::Brotli10 => Self::Brotli(BrotliCompressor::new(lvl)),
            lvl @ CompressionAlgo::Brotli11 => Self::Brotli(BrotliCompressor::new(lvl)),
            CompressionAlgo::Zlib => Self::Zlib(ZlibCompressor),
        }
    }
}
