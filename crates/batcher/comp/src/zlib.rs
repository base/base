//! Contains ZLIB compression and decompression primitives for Base.

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use miniz_oxide::inflate::{DecompressError, decompress_to_vec_zlib};

use crate::{CompressorResult, CompressorWriter};

/// The best compression level for ZLIB.
const BEST_ZLIB_COMPRESSION: u8 = 9;

/// The ZLIB compressor.
///
/// Raw input bytes are accumulated on every [`CompressorWriter::write`] call
/// without compressing them. Compression is deferred until [`compressed_len`]
/// or
/// [`CompressorWriter::read`] is called, at which point the entire accumulated
/// buffer is compressed once and cached until the next write invalidates it.
///
/// [`compressed_len`]: CompressorWriter::compressed_len
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ZlibCompressor {
    /// Holds a non-compressed buffer.
    buffer: Vec<u8>,
    /// The lazily-materialised compressed buffer.  Valid only when `dirty` is
    /// `false`.
    compressed: RefCell<Vec<u8>>,
    /// Set to `true` when `buffer` has been extended since the last
    /// compression run.
    dirty: Cell<bool>,
    /// Offset of the next unread compressed byte.
    read_offset: Cell<usize>,
}

impl ZlibCompressor {
    /// Create a new ZLIB compressor.
    pub const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            compressed: RefCell::new(Vec::new()),
            dirty: Cell::new(false),
            read_offset: Cell::new(0),
        }
    }

    /// Compress `data` using ZLIB deflate.
    pub fn compress(data: &[u8]) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec_zlib(data, BEST_ZLIB_COMPRESSION)
    }

    /// Decompress ZLIB-deflated `data`.
    pub fn decompress(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
        decompress_to_vec_zlib(data)
    }

    /// Compresses `buffer` into `compressed` if the dirty flag is set, then
    /// clears the flag.
    fn ensure_compressed(&self) {
        if self.dirty.get() {
            *self.compressed.borrow_mut() = Self::compress(&self.buffer);
            self.dirty.set(false);
            self.read_offset.set(0);
        }
    }
}

impl CompressorWriter for ZlibCompressor {
    fn write(&mut self, data: &[u8]) -> CompressorResult<usize> {
        // Accumulate raw bytes without compressing.  Compression is deferred
        // to the first call that actually needs the compressed output.
        self.buffer.extend_from_slice(data);
        self.compressed.borrow_mut().clear();
        self.dirty.set(true);
        self.read_offset.set(0);
        Ok(data.len())
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.compressed.borrow_mut().clear();
        self.dirty.set(false);
        self.read_offset.set(0);
    }

    fn compressed_len(&self) -> CompressorResult<usize> {
        self.ensure_compressed();
        Ok(self.compressed.borrow().len().saturating_sub(self.read_offset.get()))
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        self.ensure_compressed();
        let compressed = self.compressed.borrow();
        let offset = self.read_offset.get();
        let len = compressed.len().saturating_sub(offset).min(buf.len());
        buf[..len].copy_from_slice(&compressed[offset..offset + len]);
        self.read_offset.set(offset + len);
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn test_read_drains_zlib_stream() {
        let input = b"batch channel data";
        let mut compressor = ZlibCompressor::new();
        compressor.write(input).unwrap();

        let mut compressed = Vec::new();
        while compressor.compressed_len().unwrap() > 0 {
            let mut chunk = vec![0; 3];
            let read = compressor.read(&mut chunk).unwrap();
            compressed.extend_from_slice(&chunk[..read]);
        }

        assert_eq!(compressor.read(&mut [0; 1]).unwrap(), 0);
        assert_eq!(compressor.compressed_len(), Ok(0));
        assert_eq!(ZlibCompressor::decompress(&compressed).unwrap(), input);
    }
}
