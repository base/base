//! Compression algorithm and error types.

use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use crate::brotli::BrotliCompressor;

/// A channel compression failure.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// Zlib rejected an incremental compression operation.
    #[cfg(feature = "std")]
    #[error("zlib compression failed")]
    Zlib,
    /// Brotli compression is unavailable without the standard library.
    #[cfg(not(feature = "std"))]
    #[error("brotli compression is not supported without the standard library")]
    BrotliUnavailable,
    /// Brotli compression failed.
    #[cfg(feature = "std")]
    #[error("brotli compression failed: {0}")]
    Brotli(#[from] std::io::Error),
    /// Brotli quality is outside the encoder's accepted range.
    #[error("brotli quality {quality} is outside 0..=11")]
    InvalidBrotliQuality {
        /// The configured Brotli quality.
        quality: u8,
    },
}

/// The compression algorithm type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionAlgo {
    /// Brotli with quality `0..=11`.
    Brotli(u8),
    /// The zlib compression.
    Zlib,
}

impl CompressionAlgo {
    /// Inclusive minimum Brotli quality accepted by the encoder.
    pub const BROTLI_MIN_QUALITY: u8 = 0;
    /// Inclusive maximum Brotli quality accepted by the encoder.
    pub const BROTLI_MAX_QUALITY: u8 = 11;
    /// Default Brotli quality used by the batcher.
    pub const BROTLI_DEFAULT_QUALITY: u8 = 10;

    /// Channel-version byte prepended to Brotli-compressed channels.
    pub const BROTLI_CHANNEL_VERSION: u8 = 0x01;

    /// Brotli at `quality`, or `None` when it is outside `0..=11`.
    pub const fn brotli(quality: u8) -> Option<Self> {
        if quality <= Self::BROTLI_MAX_QUALITY { Some(Self::Brotli(quality)) } else { None }
    }

    /// Compresses one complete derivation channel.
    ///
    /// Brotli output starts with [`BROTLI_CHANNEL_VERSION`](Self::BROTLI_CHANNEL_VERSION);
    /// zlib is self-identifying.
    pub fn compress_channel(self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        let quality = match self {
            Self::Zlib => return Ok(miniz_oxide::deflate::compress_to_vec_zlib(input, 9)),
            Self::Brotli(quality) => {
                if quality > Self::BROTLI_MAX_QUALITY {
                    return Err(CompressionError::InvalidBrotliQuality { quality });
                }
                i32::from(quality)
            }
        };

        #[cfg(feature = "std")]
        {
            let compressed = BrotliCompressor::compress(input, quality)?;
            let mut channel = Vec::with_capacity(compressed.len() + 1);
            channel.push(Self::BROTLI_CHANNEL_VERSION);
            channel.extend_from_slice(&compressed);
            Ok(channel)
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = (input, quality);
            Err(CompressionError::BrotliUnavailable)
        }
    }
}

impl fmt::Display for CompressionAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zlib => f.write_str("zlib"),
            Self::Brotli(quality) => write!(f, "brotli-{quality}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "std")]
    use base_common_genesis::RollupConfig;
    #[cfg(feature = "std")]
    use base_protocol::Brotli;
    use miniz_oxide::inflate::decompress_to_vec_zlib;

    use super::*;

    #[test]
    fn zlib_channel_is_self_identifying() {
        let input = b"batch channel data";
        let channel = CompressionAlgo::Zlib.compress_channel(input).unwrap();

        assert_eq!(decompress_to_vec_zlib(&channel).unwrap(), input);
    }

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_has_version_prefix() {
        let channel = CompressionAlgo::Brotli(10).compress_channel(b"batch channel data").unwrap();

        assert_eq!(channel.first(), Some(&CompressionAlgo::BROTLI_CHANNEL_VERSION));
    }

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_roundtrips() {
        let input = b"batch channel data";
        let channel = CompressionAlgo::Brotli(10).compress_channel(input).unwrap();
        let decompressed = Brotli
            .decompress(&channel[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn brotli_rejects_quality_above_max() {
        let err = CompressionAlgo::Brotli(12).compress_channel(b"batch channel data").unwrap_err();

        assert!(matches!(err, CompressionError::InvalidBrotliQuality { quality: 12 }));
    }

    #[test]
    fn brotli_constructor_accepts_inclusive_range() {
        assert_eq!(CompressionAlgo::brotli(0), Some(CompressionAlgo::Brotli(0)));
        assert_eq!(CompressionAlgo::brotli(11), Some(CompressionAlgo::Brotli(11)));
        assert_eq!(CompressionAlgo::brotli(12), None);
    }
}
