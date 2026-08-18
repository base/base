//! Compression algorithm and error types.

use alloc::vec::Vec;

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
}

/// The compression algorithm type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionAlgo {
    /// The fastest brotli compression level.
    Brotli9,
    /// The default brotli compression level.
    Brotli10,
    /// The best brotli compression level.
    Brotli11,
    /// The zlib compression.
    Zlib,
}

impl CompressionAlgo {
    /// Compresses one complete derivation channel.
    ///
    /// Brotli channels carry the `0x01` channel-version byte before the
    /// compressed stream. Zlib streams are self-identifying and need no prefix.
    pub fn compress_channel(self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        let quality = match self {
            Self::Zlib => return Ok(miniz_oxide::deflate::compress_to_vec_zlib(input, 9)),
            Self::Brotli9 => 9,
            Self::Brotli10 => 10,
            Self::Brotli11 => 11,
        };

        #[cfg(feature = "std")]
        {
            let compressed = BrotliCompressor::compress(input, quality)?;
            let mut channel = Vec::with_capacity(compressed.len() + 1);
            channel.push(0x01);
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

#[cfg(test)]
mod tests {
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
        let channel = CompressionAlgo::Brotli10.compress_channel(b"batch channel data").unwrap();

        assert_eq!(channel.first(), Some(&0x01));
    }
}
