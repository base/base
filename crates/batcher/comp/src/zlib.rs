//! Contains ZLIB compression and decompression primitives for Base.

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use miniz_oxide::inflate::DecompressError;

use crate::{ChannelCompressor, CompressorResult, CompressorWriter};

/// The best compression level for ZLIB.
const BEST_ZLIB_COMPRESSION: u8 = 9;

/// The ZLIB compressor.
///
/// Raw input bytes are accumulated on every [`CompressorWriter::write`] call
/// without compressing them.  Compression is deferred until [`len`],
/// [`ChannelCompressor::get_compressed`], or [`CompressorWriter::read`] is
/// called, at which point the entire accumulated buffer is compressed once and
/// the result is cached.  Subsequent queries return the cached value in O(1)
/// until the next write invalidates it.
///
/// [`len`]: CompressorWriter::len
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
}

impl ZlibCompressor {
    /// Create a new ZLIB compressor.
    pub const fn new() -> Self {
        Self { buffer: Vec::new(), compressed: RefCell::new(Vec::new()), dirty: Cell::new(false) }
    }

    /// Compress `data` using ZLIB deflate.
    pub fn compress(data: &[u8]) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec(data, BEST_ZLIB_COMPRESSION)
    }

    /// Decompress ZLIB-deflated `data`.
    pub fn decompress(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
        miniz_oxide::inflate::decompress_to_vec(data)
    }

    /// Compresses `buffer` into `compressed` if the dirty flag is set, then
    /// clears the flag.
    fn ensure_compressed(&self) {
        if self.dirty.get() {
            *self.compressed.borrow_mut() = Self::compress(&self.buffer);
            self.dirty.set(false);
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
        Ok(data.len())
    }

    fn flush(&mut self) -> CompressorResult<()> {
        Ok(())
    }

    fn close(&mut self) -> CompressorResult<()> {
        Ok(())
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.compressed.borrow_mut().clear();
        self.dirty.set(false);
    }

    fn len(&self) -> usize {
        self.ensure_compressed();
        self.compressed.borrow().len()
    }

    fn read(&mut self, buf: &mut [u8]) -> CompressorResult<usize> {
        self.ensure_compressed();
        let compressed = self.compressed.borrow();
        let len = compressed.len().min(buf.len());
        buf[..len].copy_from_slice(&compressed[..len]);
        Ok(len)
    }
}

impl ChannelCompressor for ZlibCompressor {
    fn get_compressed(&self) -> Vec<u8> {
        self.ensure_compressed();
        self.compressed.borrow().clone()
    }
}
