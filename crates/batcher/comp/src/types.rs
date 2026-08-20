//! Compression types.

use alloc::vec::Vec;

/// The result from compressing data.
pub type CompressorResult<T> = Result<T, CompressorError>;

/// An error returned by the compressor.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum CompressorError {
    /// Thrown when the compressor is full.
    #[error("compressor is full")]
    Full,
    /// Brotli compression failed.
    #[error("brotli compression failed")]
    Brotli,
}

/// Failure from one-shot or streaming channel compression.
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
///
/// See:
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
    /// Channel-version byte prepended to Brotli-compressed channels.
    pub const BROTLI_CHANNEL_VERSION: u8 = 0x01;

    /// Compresses one complete derivation channel.
    ///
    /// Brotli output starts with [`BROTLI_CHANNEL_VERSION`](Self::BROTLI_CHANNEL_VERSION);
    /// zlib is self-identifying.
    pub fn compress_channel(self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        match self {
            Self::Zlib => Ok(miniz_oxide::deflate::compress_to_vec_zlib(input, 9)),
            #[cfg(feature = "std")]
            brotli => {
                let compressed = crate::BrotliCompressor::compress(input, brotli.into()).map_err(
                    |err| match err {
                        crate::BrotliCompressionError::CompressionError(io) => {
                            CompressionError::Brotli(io)
                        }
                        crate::BrotliCompressionError::NoStd => unreachable!(),
                    },
                )?;
                let mut channel = Vec::with_capacity(compressed.len() + 1);
                channel.push(Self::BROTLI_CHANNEL_VERSION);
                channel.extend_from_slice(&compressed);
                Ok(channel)
            }
            #[cfg(not(feature = "std"))]
            Self::Brotli9 | Self::Brotli10 | Self::Brotli11 => {
                let _ = input;
                Err(CompressionError::BrotliUnavailable)
            }
        }
    }
}

#[cfg(feature = "std")]
impl<A: alloc::borrow::Borrow<CompressionAlgo>> From<A> for crate::BrotliLevel {
    fn from(algo: A) -> Self {
        match algo.borrow() {
            CompressionAlgo::Brotli9 => Self::Brotli9,
            CompressionAlgo::Brotli11 => Self::Brotli11,
            _ => Self::Brotli10,
        }
    }
}

#[cfg(test)]
mod tests {
    use miniz_oxide::inflate::decompress_to_vec_zlib;

    use super::*;

    #[cfg(feature = "std")]
    use base_common_genesis::RollupConfig;
    #[cfg(feature = "std")]
    use base_protocol::Brotli;

    #[test]
    fn zlib_channel_roundtrips() {
        let input = b"batch channel data";
        let channel = CompressionAlgo::Zlib.compress_channel(input).unwrap();
        assert_eq!(decompress_to_vec_zlib(&channel).unwrap(), input);
    }

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_has_version_prefix() {
        let channel = CompressionAlgo::Brotli10.compress_channel(b"batch channel data").unwrap();
        assert_eq!(channel.first(), Some(&CompressionAlgo::BROTLI_CHANNEL_VERSION));
    }

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_roundtrips() {
        let input = b"batch channel data";
        let channel = CompressionAlgo::Brotli10.compress_channel(input).unwrap();
        let decompressed = Brotli
            .decompress(&channel[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, input);
    }
}
