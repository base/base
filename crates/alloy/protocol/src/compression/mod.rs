//! Contains compression and decompression primitives for Optimism.

mod variant;
pub use variant::VariantCompressor;

mod types;
pub use types::{CompressionAlgo, CompressorType};

mod zlib;
pub use zlib::{compress_zlib, decompress_zlib, ZlibCompressor};

mod brotli;
pub use brotli::{
    compress_brotli, decompress_brotli, BatchDecompressionError, BrotliCompressionError,
    BrotliCompressor, BrotliLevel,
};

mod traits;
pub use traits::Compressor;

mod shadow;
pub use shadow::ShadowCompressor;

mod ratio;
pub use ratio::RatioCompressor;
