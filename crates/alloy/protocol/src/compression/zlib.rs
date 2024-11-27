//! Contains ZLIB compression and decompression primitives for Optimism.

use crate::Compressor;
use alloc::vec::Vec;
use miniz_oxide::inflate::DecompressError;

/// The best compression.
const BEST_ZLIB_COMPRESSION: u8 = 9;

/// The ZLIB compressor.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ZlibCompressor;

impl Compressor for ZlibCompressor {
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        compress_zlib(data)
    }
}

/// Method to compress data using ZLIB.
pub fn compress_zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(data, BEST_ZLIB_COMPRESSION)
}

/// Method to decompress data using ZLIB.
pub fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    miniz_oxide::inflate::decompress_to_vec(data)
}
