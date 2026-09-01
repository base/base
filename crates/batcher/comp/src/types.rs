//! Compression algorithm and error types.

use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use crate::brotli::BrotliCompressor;

/// A channel compression failure.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// Brotli compression is unavailable without the standard library.
    #[cfg(not(feature = "std"))]
    #[error("brotli compression is not supported without the standard library")]
    BrotliUnavailable,
    /// Brotli compression failed.
    #[cfg(feature = "std")]
    #[error("brotli compression failed: {0}")]
    Brotli(#[from] std::io::Error),
}

/// Brotli encoder quality.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrotliLevel {
    /// Quality level 0.
    Brotli0 = 0,
    /// Quality level 1.
    Brotli1 = 1,
    /// Quality level 2.
    Brotli2 = 2,
    /// Quality level 3.
    Brotli3 = 3,
    /// Quality level 4.
    Brotli4 = 4,
    /// Quality level 5.
    Brotli5 = 5,
    /// Quality level 6.
    Brotli6 = 6,
    /// Quality level 7.
    Brotli7 = 7,
    /// Quality level 8.
    Brotli8 = 8,
    /// Quality level 9.
    Brotli9 = 9,
    /// Quality level 10.
    Brotli10 = 10,
    /// Quality level 11.
    Brotli11 = 11,
}

impl BrotliLevel {
    /// Default quality used by the batcher.
    pub const DEFAULT: Self = Self::Brotli10;

    /// Prepended to Brotli channels so `BatchReader` selects Brotli decompression.
    pub const CHANNEL_VERSION: u8 = 0x01;

    /// Returns the numeric quality expected by Brotli encoders.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Converts a numeric Brotli quality to its named level.
    pub const fn from_u8(quality: u8) -> Option<Self> {
        Some(match quality {
            0 => Self::Brotli0,
            1 => Self::Brotli1,
            2 => Self::Brotli2,
            3 => Self::Brotli3,
            4 => Self::Brotli4,
            5 => Self::Brotli5,
            6 => Self::Brotli6,
            7 => Self::Brotli7,
            8 => Self::Brotli8,
            9 => Self::Brotli9,
            10 => Self::Brotli10,
            11 => Self::Brotli11,
            _ => return None,
        })
    }

    /// Compresses one complete Brotli derivation channel.
    pub fn compress_channel(self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        #[cfg(feature = "std")]
        {
            let compressed = BrotliCompressor::compress(input, self)?;
            let mut channel = Vec::with_capacity(compressed.len() + 1);
            channel.push(Self::CHANNEL_VERSION);
            channel.extend_from_slice(&compressed);
            Ok(channel)
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = (input, self);
            Err(CompressionError::BrotliUnavailable)
        }
    }
}

impl fmt::Display for BrotliLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "std")]
    use base_common_genesis::RollupConfig;
    #[cfg(feature = "std")]
    use base_protocol::Brotli;

    use super::*;

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_has_version_prefix() {
        let channel = BrotliLevel::Brotli10.compress_channel(b"batch channel data").unwrap();

        assert_eq!(channel.first(), Some(&BrotliLevel::CHANNEL_VERSION));
    }

    #[cfg(feature = "std")]
    #[test]
    fn brotli_channel_roundtrips_at_quality_bounds() {
        let input = b"batch channel data";
        for level in [BrotliLevel::Brotli0, BrotliLevel::Brotli11] {
            let channel = level.compress_channel(input).unwrap();
            let decompressed = Brotli
                .decompress(&channel[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
                .unwrap();
            assert_eq!(decompressed, input);
        }
    }

    #[test]
    fn brotli_levels_cover_the_full_encoder_range() {
        let levels = [
            BrotliLevel::Brotli0,
            BrotliLevel::Brotli1,
            BrotliLevel::Brotli2,
            BrotliLevel::Brotli3,
            BrotliLevel::Brotli4,
            BrotliLevel::Brotli5,
            BrotliLevel::Brotli6,
            BrotliLevel::Brotli7,
            BrotliLevel::Brotli8,
            BrotliLevel::Brotli9,
            BrotliLevel::Brotli10,
            BrotliLevel::Brotli11,
        ];

        for (quality, level) in (0u8..=11).zip(levels) {
            assert_eq!(BrotliLevel::from_u8(quality), Some(level));
            assert_eq!(level.as_u32(), u32::from(quality));
        }
        assert_eq!(BrotliLevel::from_u8(12), None);
    }
}
