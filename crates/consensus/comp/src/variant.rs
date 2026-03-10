//! A variant over the different implementations of [`ChannelCompressor`].

use ambassador::Delegate;
use base_consensus_genesis::RollupConfig;

use crate::{
    BrotliCompressor, ChannelCompressor, CompressionAlgo, CompressorResult, CompressorWriter,
    ZlibCompressor,
    traits::{ambassador_impl_ChannelCompressor, ambassador_impl_CompressorWriter},
};

/// The channel compressor wraps the brotli and zlib compressor types,
/// implementing the [`ChannelCompressor`] trait itself.
#[derive(Debug, Clone, Delegate)]
#[allow(clippy::duplicated_attributes)]
#[delegate(CompressorWriter)]
#[delegate(ChannelCompressor)]
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
