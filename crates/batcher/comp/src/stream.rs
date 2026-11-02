//! Incremental derivation-channel compression.

use alloc::{boxed::Box, vec::Vec};
use core::num::NonZeroUsize;
use std::io::Write;

use brotli::{CompressorWriter, enc::BrotliEncoderMaxCompressedSize};

use crate::{BrotliLevel, CompressionError};

/// A single incremental compressor for one derivation channel.
#[derive(derive_more::Debug)]
pub struct CompressionStream {
    /// Streaming Brotli encoder writing into its transferable output buffer.
    #[debug(skip)]
    compressor: Box<CompressorWriter<Vec<u8>>>,
    /// Total bytes returned by prior appends.
    output_size: usize,
    /// Input bytes after which an append flushes the encoder, if any.
    flush_interval: Option<NonZeroUsize>,
    /// Input bytes appended since the last flush.
    unflushed_input: usize,
}

impl CompressionStream {
    /// Buffer size used by the Brotli writer.
    const BROTLI_BUFFER_SIZE: usize = 4096;

    /// Brotli sliding-window exponent (`2^22` bytes).
    const BROTLI_LGWIN: u32 = 22;

    /// Worst-case bytes one flush adds: it ends a meta-block early, which costs one
    /// more meta-block header (4 bytes), and pads it to a byte boundary (2 bytes).
    const FLUSH_OVERHEAD: usize = 6;

    /// Creates an empty compressor at `level` that flushes every `flush_interval`
    /// input bytes, or never when `None`.
    ///
    /// Unflushed, Brotli holds up to about one window of input before emitting
    /// anything, so [`output_size`](Self::output_size) can trail the real
    /// compressed size by that much input. A flush keeps the window, so it costs
    /// only a meta-block boundary.
    pub fn new(level: BrotliLevel, flush_interval: Option<NonZeroUsize>) -> Self {
        let mut output = Vec::with_capacity(Self::BROTLI_BUFFER_SIZE);
        output.push(BrotliLevel::CHANNEL_VERSION);
        let compressor = CompressorWriter::new(
            output,
            Self::BROTLI_BUFFER_SIZE,
            level.as_u32(),
            Self::BROTLI_LGWIN,
        );
        Self {
            compressor: Box::new(compressor),
            output_size: 0,
            flush_interval,
            unflushed_input: 0,
        }
    }

    /// Append input and return newly emitted compressed bytes.
    ///
    /// Flushes once the flush interval's worth of input has been appended since the
    /// last flush, so the bytes returned so far always cover all but less than one
    /// flush interval of the input.
    pub fn append(&mut self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        self.compressor.write_all(input)?;
        self.unflushed_input += input.len();
        if self.flush_interval.is_some_and(|interval| self.unflushed_input >= interval.get()) {
            self.compressor.flush()?;
            self.unflushed_input = 0;
        }
        let output = core::mem::take(self.compressor.get_mut());
        self.output_size += output.len();
        Ok(output)
    }

    /// Bytes already returned by [`append`](Self::append), excluding any `finish` suffix.
    pub const fn output_size(&self) -> usize {
        self.output_size
    }

    /// Returns a conservative upper bound for a finished stream of `input_size`.
    pub fn max_output_size(&self, input_size: usize) -> usize {
        // Each flush follows at least one flush interval of new input.
        let max_flushes = self.flush_interval.map_or(0, |interval| input_size / interval);
        BrotliEncoderMaxCompressedSize(input_size)
            .saturating_add(max_flushes.saturating_mul(Self::FLUSH_OVERHEAD))
            // Channel-version prefix byte.
            .saturating_add(1)
    }

    /// Finishes the stream and returns compressed bytes not previously transferred.
    pub fn finish(self) -> Result<Vec<u8>, CompressionError> {
        // Consuming the writer emits Brotli's stream trailer.
        Ok((*self.compressor).into_inner())
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::RollupConfig;
    use base_protocol::Brotli;

    use super::*;

    const CHUNKS: [&[u8]; 3] = [b"first batch", b"second batch", b"third batch"];

    #[test]
    fn brotli_roundtrip_across_appends() {
        let expected = CHUNKS.concat();
        let mut compressor = CompressionStream::new(BrotliLevel::Brotli10, None);
        let mut compressed = Vec::new();

        for chunk in CHUNKS {
            compressed.extend(compressor.append(chunk).unwrap());
        }
        compressed.extend(compressor.finish().unwrap());

        assert_eq!(compressed.first(), Some(&BrotliLevel::CHANNEL_VERSION));
        let decompressed = Brotli
            .decompress(&compressed[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn chunking_does_not_change_the_compressed_stream() {
        let input = CHUNKS.concat();
        let level = BrotliLevel::Brotli10;
        let mut chunked = CompressionStream::new(level, None);
        let mut chunked_output = Vec::new();
        for chunk in CHUNKS {
            chunked_output.extend(chunked.append(chunk).unwrap());
        }
        chunked_output.extend(chunked.finish().unwrap());

        let mut single = CompressionStream::new(level, None);
        let mut single_output = single.append(&input).unwrap();
        single_output.extend(single.finish().unwrap());

        assert_eq!(chunked_output, single_output);
    }

    #[test]
    fn reported_bounds_cover_incompressible_streams() {
        let mut state = 1u64;
        let input: Vec<u8> = (0..300_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();

        for flush_interval in [None, NonZeroUsize::new(1), NonZeroUsize::new(100_000)] {
            let mut compressor = CompressionStream::new(BrotliLevel::Brotli10, flush_interval);
            let bound = compressor.max_output_size(input.len());
            let mut output_size = 0usize;
            for chunk in input.chunks(7919) {
                output_size += compressor.append(chunk).unwrap().len();
            }
            output_size += compressor.finish().unwrap().len();
            assert!(output_size <= bound, "flush interval {flush_interval:?}");
        }
    }
}
