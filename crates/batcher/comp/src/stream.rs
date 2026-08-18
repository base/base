//! Incremental derivation-channel compression.

use alloc::{boxed::Box, vec::Vec};
use std::{fmt, io::Write};

use brotli::{CompressorWriter, enc::BrotliEncoderMaxCompressedSize};
use miniz_oxide::{
    DataFormat,
    deflate::{
        CompressionLevel,
        core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress_to_output},
    },
};

use crate::{CompressionAlgo, CompressionError};

/// Concrete state owned by one [`CompressionStream`].
pub enum CompressionBackend {
    /// Streaming zlib encoder and bytes emitted since the last transfer.
    Zlib {
        /// Miniz encoder state.
        compressor: Box<CompressorOxide>,
        /// Bytes emitted since the last transfer.
        output: Vec<u8>,
    },
    /// Streaming Brotli encoder writing into its transferable output buffer.
    Brotli(Box<CompressorWriter<Vec<u8>>>),
}

impl fmt::Debug for CompressionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zlib { output, .. } => {
                f.debug_struct("Zlib").field("buffered_bytes", &output.len()).finish()
            }
            Self::Brotli(compressor) => f
                .debug_struct("Brotli")
                .field("buffered_bytes", &compressor.get_ref().len())
                .finish(),
        }
    }
}

/// A single incremental compressor for one derivation channel.
pub struct CompressionStream {
    /// Selected streaming backend.
    backend: CompressionBackend,
    /// Total bytes returned by prior appends.
    output_size: usize,
}

impl CompressionStream {
    /// Creates an empty compressor for `algorithm`.
    pub fn new(algorithm: CompressionAlgo) -> Self {
        let brotli = |quality| {
            let mut output = Vec::with_capacity(4096);
            output.push(0x01);
            CompressionBackend::Brotli(Box::new(CompressorWriter::new(output, 4096, quality, 22)))
        };
        let backend = match algorithm {
            CompressionAlgo::Zlib => CompressionBackend::Zlib {
                compressor: Box::new(CompressorOxide::with_format_and_level(
                    DataFormat::Zlib,
                    CompressionLevel::BestCompression,
                )),
                output: Vec::new(),
            },
            CompressionAlgo::Brotli9 => brotli(9),
            CompressionAlgo::Brotli10 => brotli(10),
            CompressionAlgo::Brotli11 => brotli(11),
        };
        Self { backend, output_size: 0 }
    }

    /// Appends input and returns newly observable compressed bytes.
    ///
    /// The returned bytes are an immutable continuation of earlier output. The
    /// stream retains its dictionary and does not flush.
    pub fn append(&mut self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        // miniz exposes explicit stream progress, so validate that every byte
        // was consumed before treating the append as committed.
        match &mut self.backend {
            CompressionBackend::Zlib { compressor, output } => {
                let (status, consumed) =
                    compress_to_output(compressor, input, TDEFLFlush::None, |bytes| {
                        output.extend_from_slice(bytes);
                        true
                    });
                if status != TDEFLStatus::Okay || consumed != input.len() {
                    return Err(CompressionError::Zlib);
                }
            }
            CompressionBackend::Brotli(compressor) => compressor.write_all(input)?,
        }

        let output = match &mut self.backend {
            CompressionBackend::Zlib { output, .. } => core::mem::take(output),
            CompressionBackend::Brotli(compressor) => core::mem::take(compressor.get_mut()),
        };
        self.output_size = self.output_size.saturating_add(output.len());
        Ok(output)
    }

    /// Bytes already returned by [`append`](Self::append), excluding any `finish` suffix.
    pub const fn output_size(&self) -> usize {
        self.output_size
    }

    /// Returns a conservative upper bound for a finished stream of `input_size`.
    pub fn max_output_size(&self, input_size: usize) -> usize {
        match &self.backend {
            CompressionBackend::Zlib { .. } => {
                // This is miniz's `mz_compressBound` formula.
                input_size.saturating_add(input_size / 16).saturating_add(67)
            }
            CompressionBackend::Brotli(_) => {
                // Account for the derivation channel-version byte.
                BrotliEncoderMaxCompressedSize(input_size).saturating_add(1)
            }
        }
    }

    /// Finishes the stream and returns compressed bytes not previously transferred.
    pub fn finish(self) -> Result<Vec<u8>, CompressionError> {
        // Finalize zlib explicitly so trailer bytes are included and failures
        // remain observable to the channel state machine.
        match self.backend {
            CompressionBackend::Zlib { mut compressor, mut output } => {
                let (status, consumed) =
                    compress_to_output(&mut compressor, &[], TDEFLFlush::Finish, |bytes| {
                        output.extend_from_slice(bytes);
                        true
                    });
                if status != TDEFLStatus::Done || consumed != 0 {
                    return Err(CompressionError::Zlib);
                }
                Ok(output)
            }
            // Consuming CompressorWriter emits Brotli's final stream bytes.
            CompressionBackend::Brotli(compressor) => Ok((*compressor).into_inner()),
        }
    }
}

impl fmt::Debug for CompressionStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompressionStream")
            .field("backend", &self.backend)
            .field("output_size", &self.output_size())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::RollupConfig;
    use base_protocol::Brotli;
    use miniz_oxide::inflate::decompress_to_vec_zlib;

    use super::*;

    const CHUNKS: [&[u8]; 3] = [b"first batch", b"second batch", b"third batch"];

    #[test]
    fn zlib_roundtrip_across_appends() {
        let expected = CHUNKS.concat();
        let mut compressor = CompressionStream::new(CompressionAlgo::Zlib);
        let mut compressed = Vec::new();

        for chunk in CHUNKS {
            compressed.extend(compressor.append(chunk).unwrap());
        }
        compressed.extend(compressor.finish().unwrap());

        assert_eq!(decompress_to_vec_zlib(&compressed).unwrap(), expected);
    }

    #[test]
    fn brotli_roundtrip_across_appends() {
        let expected = CHUNKS.concat();
        let mut compressor = CompressionStream::new(CompressionAlgo::Brotli10);
        let mut compressed = Vec::new();

        for chunk in CHUNKS {
            compressed.extend(compressor.append(chunk).unwrap());
        }
        compressed.extend(compressor.finish().unwrap());

        assert_eq!(compressed.first(), Some(&0x01));
        let decompressed = Brotli
            .decompress(&compressed[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn chunking_does_not_change_the_compressed_stream() {
        let input = CHUNKS.concat();

        for algorithm in [CompressionAlgo::Zlib, CompressionAlgo::Brotli10] {
            let mut chunked = CompressionStream::new(algorithm);
            let mut chunked_output = Vec::new();
            for chunk in CHUNKS {
                chunked_output.extend(chunked.append(chunk).unwrap());
            }
            chunked_output.extend(chunked.finish().unwrap());

            let mut single = CompressionStream::new(algorithm);
            let mut single_output = single.append(&input).unwrap();
            single_output.extend(single.finish().unwrap());

            assert_eq!(chunked_output, single_output);
        }
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

        for algorithm in [CompressionAlgo::Zlib, CompressionAlgo::Brotli10] {
            let mut compressor = CompressionStream::new(algorithm);
            let bound = compressor.max_output_size(input.len());
            let mut output_size = 0usize;
            for chunk in input.chunks(7919) {
                output_size = output_size.saturating_add(compressor.append(chunk).unwrap().len());
            }
            output_size = output_size.saturating_add(compressor.finish().unwrap().len());
            assert!(output_size <= bound);
        }
    }
}
