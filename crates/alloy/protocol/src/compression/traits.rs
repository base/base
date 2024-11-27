//! Contains the core `Compressor` trait.

use alloc::vec::Vec;

/// The Compressor trait abstracts compression.
pub trait Compressor {
    /// Compresses the given data.
    fn compress(&self, data: &[u8]) -> Vec<u8>;
}
