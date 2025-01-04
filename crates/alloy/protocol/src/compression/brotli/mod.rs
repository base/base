//! Contains brotli compression and decompression utilities.

#[cfg(feature = "std")]
mod compress;
#[cfg(feature = "std")]
pub use compress::{BrotliCompressionError, BrotliCompressor};

mod decompress;
pub use decompress::{decompress_brotli, BrotliDecompressionError};

/// The brotli encoding level used in Optimism.
///
/// See: <https://github.com/ethereum-optimism/optimism/blob/develop/op-node/rollup/derive/types.go#L50>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrotliLevel {
    /// The fastest compression level.
    Brotli9 = 9,
    /// The default compression level.
    Brotli10 = 10,
    /// The highest compression level.
    Brotli11 = 11,
}

impl From<BrotliLevel> for u32 {
    fn from(level: BrotliLevel) -> Self {
        level as Self
    }
}
