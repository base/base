//! Contains the shadow compressor for Base.
//!
//! This is a port of the reference batcher's shadow compressor.

use alloc::vec::Vec;

use crate::{
    ChannelCompressor, CompressorError, CompressorResult, CompressorWriter, Config,
    VariantCompressor,
};

/// The largest potential blow-up in bytes we expect to see when compressing
/// arbitrary (e.g. random) data.  Here we account for a 2 byte header, 4 byte
/// digest, 5 byte eof indicator, and then 5 byte flate block header for each 16k of potential
/// data. Assuming frames are max 128k size (the current max blob size) this is 2+4+5+(5*8) = 51
/// bytes.  If we start using larger frames (e.g. should max blob size increase) a larger blowup
/// might be possible, but it would be highly unlikely, and the system still works if our
/// estimate is wrong -- we just end up writing one more tx for the overflow.
const SAFE_COMPRESSION_OVERHEAD: u64 = 51;

// The number of final bytes a `zlib.Writer` call writes to the output buffer.
const CLOSE_OVERHEAD_ZLIB: u64 = 9;

/// Shadow Compressor
///
/// The shadow compressor contains two compression buffers, one for size estimation, and
/// one for the final compressed data. The first compression buffer is flushed on every
/// write, and the second isn't, which means the final compressed data is always at least
/// smaller than the size estimation.
///
/// One exception to the rule is when the first write to the buffer is not checked against
/// the target. This allows individual blocks larger than the target to be included.
/// Notice, this will be split across multiple channel frames.
#[derive(Debug, Clone)]
pub struct ShadowCompressor {
    /// The compressor configuration.
    config: Config,
    /// The inner [`VariantCompressor`] that will be used to compress the data.
    compressor: VariantCompressor,
    /// The shadow compressor.
    shadow: VariantCompressor,

    /// Flags that the buffer is full.
    is_full: bool,
    /// An upper bound on the size of the compressed data.
    bound: u64,
}

impl ShadowCompressor {
    /// Creates a new [`ShadowCompressor`] with the given [`VariantCompressor`].
    pub const fn new(
        config: Config,
        compressor: VariantCompressor,
        shadow: VariantCompressor,
    ) -> Self {
        Self { config, is_full: false, compressor, shadow, bound: SAFE_COMPRESSION_OVERHEAD }
    }
}

impl From<Config> for ShadowCompressor {
    fn from(config: Config) -> Self {
        let compressor = VariantCompressor::from(config.compression_algo);
        let shadow = VariantCompressor::from(config.compression_algo);
        Self::new(config, compressor, shadow)
    }
}

impl CompressorWriter for ShadowCompressor {
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
        // If the buffer is full, error so the user can flush.
        if self.is_full {
            return Err(CompressorError::Full);
        }

        // Write to the shadow compressor and always flush it to obtain an
        // up-to-date compressed-size estimate.
        //
        // The previous implementation only flushed the shadow when a single
        // write's *uncompressed* size exceeded `target_output_size`.  That
        // missed the common case where many small writes collectively push the
        // total compressed output past the target: `is_full` was never set,
        // the compressor kept accepting data, and the resulting frame was
        // silently larger than `target_output_size` on-chain.
        //
        // Flushing on every write matches the docstring ("the shadow buffer is
        // flushed on every write") and ensures that `bound` always reflects
        // the cumulative compressed-size upper bound.
        self.shadow.write(data)?;
        self.shadow.flush()?;
        let newbound = self.shadow.len() as u64 + CLOSE_OVERHEAD_ZLIB;

        if newbound > self.config.target_output_size {
            self.is_full = true;
            // Only error if the main compressor already contains data.  The
            // first write is always allowed through so that a single block
            // larger than `target_output_size` can still be encoded (it will
            // be split across multiple frames by the frame encoder).
            if self.compressor.len() > 0 {
                return Err(CompressorError::Full);
            }
        }

        // Update the bound and compress.
        self.bound = newbound;
        self.compressor.write(data)
    }

    fn len(&self) -> usize {
        self.compressor.len()
    }

    fn flush(&mut self) -> CompressorResult<()> {
        // Both the shadow and main compressors use lazy compression: they
        // accumulate raw bytes on write() and only materialise the compressed
        // output when len(), get_compressed(), or read() is called.  flush()
        // is therefore a no-op on both underlying compressors, but we forward
        // it to each for API completeness.
        self.shadow.flush()?;
        self.compressor.flush()
    }

    fn close(&mut self) -> CompressorResult<()> {
        // Only the shadow compressor is closed. The main compressor does not
        // require an explicit `close()` because its `compressed` buffer is
        // always fully materialized after each `write()` — `read()` can drain
        // it directly without any finalization step. See the comment on
        // `flush()` above for more detail.
        self.shadow.close()
    }

    fn reset(&mut self) {
        self.compressor.reset();
        self.shadow.reset();
        self.is_full = false;
        self.bound = SAFE_COMPRESSION_OVERHEAD;
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        self.compressor.read(buf)
    }
}

impl ChannelCompressor for ShadowCompressor {
    fn get_compressed(&self) -> Vec<u8> {
        self.compressor.get_compressed()
    }

    fn channel_version_byte(&self) -> Option<u8> {
        self.compressor.channel_version_byte()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompressionAlgo, CompressorType};

    fn shadow_config(target: u64) -> Config {
        Config {
            target_output_size: target,
            approx_compr_ratio: 0.6,
            compression_algo: CompressionAlgo::Zlib,
            kind: CompressorType::Shadow,
        }
    }

    /// Regression: many small writes that collectively push compressed output
    /// past `target_output_size` must eventually trigger `is_full`.
    ///
    /// Before the fix the shadow compressor only flushed and checked fullness
    /// when a single write's *uncompressed* size exceeded the target. Small
    /// writes slipped through unchecked and the resulting frame was silently
    /// larger than intended.
    #[test]
    fn test_small_writes_eventually_trigger_full() {
        let target: u64 = 64;
        let config = shadow_config(target);
        let mut sc = ShadowCompressor::from(config);

        let chunk = b"hello_world__padding"; // 20 bytes — well below target
        let mut iters = 0usize;
        loop {
            match sc.write(chunk) {
                Ok(_) => {}
                Err(CompressorError::Full) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
            iters += 1;
            assert!(iters < 10_000, "compressor never became full after cumulative small writes");
        }
        assert!(sc.is_full, "is_full must be set when cumulative compressed size exceeds target");
    }

    /// A single write larger than `target_output_size` must be allowed through
    /// (first-write exception) but mark the compressor full so the next write
    /// is rejected.
    #[test]
    fn test_large_first_write_allowed_second_rejected() {
        let target: u64 = 16;
        let config = shadow_config(target);
        let mut sc = ShadowCompressor::from(config);

        let big = vec![0u8; 200];
        assert!(sc.write(&big).is_ok(), "first oversized write must succeed");
        assert!(sc.is_full, "compressor must be full after oversized first write");
        assert_eq!(sc.write(b"x"), Err(CompressorError::Full));
    }

    /// After reset() the compressor must accept writes again and fullness
    /// detection must work correctly on the fresh instance.
    #[test]
    fn test_reset_clears_full_flag_and_bound() {
        let target: u64 = 16;
        let config = shadow_config(target);
        let mut sc = ShadowCompressor::from(config);

        sc.write(&vec![0u8; 200]).unwrap();
        assert!(sc.is_full);

        sc.reset();
        assert!(!sc.is_full, "is_full must be cleared by reset");
        assert_eq!(sc.bound, SAFE_COMPRESSION_OVERHEAD, "bound must be reset to initial overhead");

        // After reset a new small write must go through cleanly.
        assert!(sc.write(b"x").is_ok());
    }
}
