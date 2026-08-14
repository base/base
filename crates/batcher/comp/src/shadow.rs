//! Contains the shadow compressor for Base.

use crate::{
    CompressionAlgo, CompressorError, CompressorResult, CompressorWriter, VariantCompressor,
};

// The number of final bytes a `zlib.Writer` call writes to the output buffer.
const CLOSE_OVERHEAD_ZLIB: u64 = 9;

/// Shadow Compressor
///
/// Maintains the channel's compressed output plus a shadow copy used for size
/// checks in the Single producer.
///
/// This preserves the current Single behavior: the compressed target is checked
/// only when an individual write is larger than the target. A sequence of
/// smaller writes is not cumulatively bounded. The Span producer does not use
/// this compressor, and the limitation remains until the Single producer is
/// removed.
///
/// An oversized first write is accepted and marks the compressor full so one
/// large block can make progress; framing later splits it across multiple frames.
#[derive(Debug, Clone)]
pub struct ShadowCompressor {
    /// Target compressed channel size.
    target_output_size: u64,
    /// The inner [`VariantCompressor`] that will be used to compress the data.
    compressor: VariantCompressor,
    /// The shadow compressor.
    shadow: VariantCompressor,

    /// Flags that the buffer is full.
    is_full: bool,
}

impl ShadowCompressor {
    /// Creates the bounded compressor used by the encoder's Single producer path.
    ///
    /// `target_output_size` is the compressed limit for the whole channel, not
    /// the size of an individual frame.
    pub fn new(target_output_size: u64, compression_algo: CompressionAlgo) -> Self {
        Self {
            target_output_size,
            compressor: VariantCompressor::from(compression_algo),
            shadow: VariantCompressor::from(compression_algo),
            is_full: false,
        }
    }
}

impl CompressorWriter for ShadowCompressor {
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
        // Once full, the channel must close before this batch can be retried.
        if self.is_full {
            return Err(CompressorError::Full);
        }

        // Write to the shadow compressor.
        self.shadow.write(data)?;

        // Preserve the current per-write check. This is not a cumulative channel bound.
        let input_size = data.len() as u64;
        if input_size > self.target_output_size {
            let output_size = self.shadow.compressed_len()? as u64 + CLOSE_OVERHEAD_ZLIB;
            if output_size > self.target_output_size {
                self.is_full = true;
                // Only error if the buffer has been written to.
                if self.compressor.compressed_len()? > 0 {
                    return Err(CompressorError::Full);
                }
            }
        }

        self.compressor.write(data)
    }

    fn compressed_len(&self) -> CompressorResult<usize> {
        self.compressor.compressed_len()
    }

    fn reset(&mut self) {
        self.compressor.reset();
        self.shadow.reset();
        self.is_full = false;
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        self.compressor.read(buf)
    }

    fn channel_version_byte(&self) -> Option<u8> {
        self.compressor.channel_version_byte()
    }
}
