//! Contains the core `Compressor` trait.

use alloc::vec::Vec;

#[cfg(feature = "std")]
use ambassador::delegatable_trait;

use crate::CompressorResult;

/// Compressor Writer
///
/// A trait that expands the standard library `Write` trait to include
/// compression-specific methods and return [`CompressorResult`] instead of
/// standard library `Result`.
#[allow(clippy::len_without_is_empty)]
#[cfg_attr(feature = "std", delegatable_trait)]
pub trait CompressorWriter {
    /// Writes the given data to the compressor.
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize>;

    /// Flushes the buffer.
    fn flush(&mut self) -> CompressorResult<()>;

    /// Closes the compressor.
    fn close(&mut self) -> CompressorResult<()>;

    /// Resets the compressor.
    fn reset(&mut self);

    /// Returns the length of the compressed data.
    fn len(&self) -> usize;

    /// Reads the compressed data into the given buffer.
    /// Returns the number of bytes read.
    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize>;
}

/// Channel Compressor
///
/// A compressor for channels.
#[cfg_attr(feature = "std", delegatable_trait)]
pub trait ChannelCompressor: CompressorWriter {
    /// Returns the compressed data buffer.
    fn get_compressed(&self) -> Vec<u8>;

    /// Returns the single-byte channel version prefix to prepend to the first
    /// frame's data, or `None` if the compression format is self-identifying
    /// (e.g. zlib, whose header bytes are recognised without a prefix).
    ///
    /// The [`BatchReader`](base_protocol::BatchReader) inspects the first byte
    /// of assembled channel data to determine the decompression algorithm.
    /// Brotli-compressed channels must start with `0x01`; zlib data is
    /// recognised by its natural header without an explicit prefix.
    fn channel_version_byte(&self) -> Option<u8> {
        None
    }
}
