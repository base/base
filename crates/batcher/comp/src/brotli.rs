//! Contains brotli compression utilities.

use alloc::vec::Vec;

use brotli::enc::{BrotliCompress, BrotliEncoderParams};

use crate::BrotliLevel;

/// Stateless Brotli compression.
#[derive(Debug)]
pub struct BrotliCompressor;

impl BrotliCompressor {
    /// Compresses the given bytes data using the Brotli compressor implemented
    /// in the [`brotli`](https://crates.io/crates/brotli) crate.
    ///
    /// `level` is the Brotli encoder quality.
    pub fn compress(mut input: &[u8], level: BrotliLevel) -> std::io::Result<Vec<u8>> {
        let mut output = alloc::vec![];
        BrotliCompress(
            &mut input,
            &mut output,
            &BrotliEncoderParams { quality: level.as_u32() as i32, ..Default::default() },
        )?;
        Ok(output)
    }
}
