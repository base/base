//! Incremental derivation-channel compression.

use alloc::{boxed::Box, vec::Vec};
use std::io::Write;

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
#[derive(derive_more::Debug)]
pub enum CompressionBackend {
    /// Streaming zlib encoder and bytes emitted since the last transfer.
    Zlib {
        /// Miniz encoder state.
        #[debug(skip)]
        compressor: Box<CompressorOxide>,
        /// Bytes emitted since the last transfer.
        #[debug(skip)]
        output: Vec<u8>,
    },
    /// Streaming Brotli encoder writing into its transferable output buffer.
    Brotli(#[debug(skip)] Box<CompressorWriter<Vec<u8>>>),
}

impl CompressionBackend {
    /// Buffer size used by the Brotli writer.
    const BROTLI_BUFFER_SIZE: usize = 4096;

    /// Brotli sliding-window exponent (`2^22` bytes).
    const BROTLI_LGWIN: u32 = 22;

    /// Appends input and returns newly emitted compressed bytes.
    pub fn append(&mut self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        match self {
            Self::Zlib { compressor, output } => {
                // miniz reports consumed input; treat the append as committed only
                // when every byte was accepted.
                let (status, consumed) =
                    compress_to_output(compressor, input, TDEFLFlush::None, |bytes| {
                        output.extend_from_slice(bytes);
                        true
                    });
                if status != TDEFLStatus::Okay || consumed != input.len() {
                    return Err(CompressionError::Zlib);
                }
                Ok(core::mem::take(output))
            }
            Self::Brotli(compressor) => {
                compressor.write_all(input)?;
                Ok(core::mem::take(compressor.get_mut()))
            }
        }
    }

    /// Returns a conservative upper bound for a finished stream of `input_size`.
    pub fn max_output_size(&self, input_size: usize) -> usize {
        match self {
            Self::Zlib { .. } => {
                // miniz `mz_compressBound`.
                input_size.saturating_add(input_size / 16).saturating_add(67)
            }
            Self::Brotli(_) => {
                // Channel-version prefix byte.
                BrotliEncoderMaxCompressedSize(input_size).saturating_add(1)
            }
        }
    }

    /// Finishes the stream and returns compressed bytes not previously transferred.
    pub fn finish(self) -> Result<Vec<u8>, CompressionError> {
        match self {
            Self::Zlib { mut compressor, mut output } => {
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
            // Consuming the writer emits Brotli's stream trailer.
            Self::Brotli(compressor) => Ok((*compressor).into_inner()),
        }
    }
}

impl From<CompressionAlgo> for CompressionBackend {
    fn from(algorithm: CompressionAlgo) -> Self {
        let brotli = |quality| {
            let mut output = Vec::with_capacity(Self::BROTLI_BUFFER_SIZE);
            output.push(CompressionAlgo::BROTLI_CHANNEL_VERSION);
            Self::Brotli(Box::new(CompressorWriter::new(
                output,
                Self::BROTLI_BUFFER_SIZE,
                quality,
                Self::BROTLI_LGWIN,
            )))
        };

        match algorithm {
            CompressionAlgo::Zlib => Self::Zlib {
                compressor: Box::new(CompressorOxide::with_format_and_level(
                    DataFormat::Zlib,
                    CompressionLevel::BestCompression,
                )),
                output: Vec::new(),
            },
            CompressionAlgo::Brotli9 => brotli(9),
            CompressionAlgo::Brotli10 => brotli(10),
            CompressionAlgo::Brotli11 => brotli(11),
        }
    }
}

/// A single incremental compressor for one derivation channel.
#[derive(derive_more::Debug)]
pub struct CompressionStream {
    backend: CompressionBackend,
    /// Total bytes returned by prior [`append`](Self::append) calls.
    output_size: usize,
}

impl From<CompressionAlgo> for CompressionStream {
    fn from(algorithm: CompressionAlgo) -> Self {
        Self { backend: algorithm.into(), output_size: 0 }
    }
}

impl CompressionStream {
    /// Creates an empty compressor for `algorithm`.
    pub fn new(algorithm: CompressionAlgo) -> Self {
        algorithm.into()
    }

    /// Append input and return newly emitted compressed bytes.
    pub fn append(&mut self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        let output = self.backend.append(input)?;
        self.output_size += output.len();
        Ok(output)
    }

    /// Bytes already returned by [`append`](Self::append), excluding any `finish` suffix.
    pub const fn output_size(&self) -> usize {
        self.output_size
    }

    /// Conservative upper bound for a finished stream of `input_size` uncompressed bytes.
    pub fn max_output_size(&self, input_size: usize) -> usize {
        self.backend.max_output_size(input_size)
    }

    /// Finishes the stream and returns compressed bytes not previously transferred.
    pub fn finish(self) -> Result<Vec<u8>, CompressionError> {
        self.backend.finish()
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::RollupConfig;
    use base_protocol::Brotli;
    use miniz_oxide::inflate::decompress_to_vec_zlib;

    use super::*;

    const CHUNKS: [&[u8]; 3] = [b"first batch", b"second batch", b"third batch"];

    fn collect(algorithm: CompressionAlgo, chunks: &[&[u8]]) -> Vec<u8> {
        let mut compressor = CompressionStream::new(algorithm);
        let mut compressed = Vec::new();
        for chunk in chunks {
            compressed.extend(compressor.append(chunk).unwrap());
        }
        compressed.extend(compressor.finish().unwrap());
        compressed
    }

    #[test]
    fn zlib_roundtrip_across_appends() {
        let expected = CHUNKS.concat();
        let compressed = collect(CompressionAlgo::Zlib, &CHUNKS);
        assert_eq!(decompress_to_vec_zlib(&compressed).unwrap(), expected);
    }

    #[test]
    fn brotli_roundtrip_across_appends() {
        let expected = CHUNKS.concat();
        let compressed = collect(CompressionAlgo::Brotli10, &CHUNKS);

        assert_eq!(compressed.first(), Some(&CompressionAlgo::BROTLI_CHANNEL_VERSION));
        let decompressed = Brotli
            .decompress(&compressed[1..], RollupConfig::MAX_RLP_BYTES_PER_CHANNEL_FJORD as usize)
            .unwrap();
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn chunking_does_not_change_the_compressed_stream() {
        let input = CHUNKS.concat();

        for algorithm in [CompressionAlgo::Zlib, CompressionAlgo::Brotli10] {
            let chunked = collect(algorithm, &CHUNKS);
            let single = collect(algorithm, &[input.as_slice()]);
            assert_eq!(chunked, single);
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
                output_size += compressor.append(chunk).unwrap().len();
            }
            output_size += compressor.finish().unwrap().len();
            assert!(output_size <= bound);
        }
    }
}
