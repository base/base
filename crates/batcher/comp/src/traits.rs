//! Contains the core `Compressor` trait.

use crate::CompressorResult;

/// Compression stream used while building and framing a channel.
///
/// Producers write RLP input and may reset the stream for an exact size
/// checkpoint. [`crate::ChannelOut::into_frames`] then drains the compressed
/// bytes and prepends the format's optional channel-version byte.
pub trait CompressorWriter {
    /// Writes the given data to the compressor.
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize>;

    /// Resets the compressor.
    fn reset(&mut self);

    /// Returns the number of compressed bytes available to read.
    fn compressed_len(&self) -> CompressorResult<usize>;

    /// Reads the compressed data into the given buffer.
    /// Returns the number of bytes read.
    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize>;

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
