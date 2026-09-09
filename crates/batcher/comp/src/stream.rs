//! Incremental derivation-channel compression.

use alloc::{boxed::Box, vec::Vec};
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
}

impl CompressionStream {
    /// Buffer size used by the Brotli writer.
    const BROTLI_BUFFER_SIZE: usize = 4096;

    /// Brotli sliding-window exponent (`2^22` bytes).
    const BROTLI_LGWIN: u32 = 22;

    /// Creates an empty compressor at `level`.
    pub fn new(level: BrotliLevel) -> Self {
        let mut output = Vec::with_capacity(Self::BROTLI_BUFFER_SIZE);
        output.push(BrotliLevel::CHANNEL_VERSION);
        let compressor = CompressorWriter::new(
            output,
            Self::BROTLI_BUFFER_SIZE,
            level.as_u32(),
            Self::BROTLI_LGWIN,
        );
        Self { compressor: Box::new(compressor), output_size: 0 }
    }

    /// Append input and return newly emitted compressed bytes.
    pub fn append(&mut self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        self.compressor.write_all(input)?;
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
        // Channel-version prefix byte.
        BrotliEncoderMaxCompressedSize(input_size).saturating_add(1)
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
        let mut compressor = CompressionStream::new(BrotliLevel::Brotli10);
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
        let mut chunked = CompressionStream::new(level);
        let mut chunked_output = Vec::new();
        for chunk in CHUNKS {
            chunked_output.extend(chunked.append(chunk).unwrap());
        }
        chunked_output.extend(chunked.finish().unwrap());

        let mut single = CompressionStream::new(level);
        let mut single_output = single.append(&input).unwrap();
        single_output.extend(single.finish().unwrap());

        assert_eq!(chunked_output, single_output);
    }

    #[test]
    fn reported_bounds_cover_incompressible_streams() {
        let mut state = 1u64;
        let input: Vec<u8> = (0..100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();

        let mut compressor = CompressionStream::new(BrotliLevel::Brotli10);
        let bound = compressor.max_output_size(input.len());
        let mut output_size = 0usize;
        for chunk in input.chunks(7919) {
            output_size += compressor.append(chunk).unwrap().len();
        }
        output_size += compressor.finish().unwrap().len();
        assert!(output_size <= bound);
    }
}
