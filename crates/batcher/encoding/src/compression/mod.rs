//! Streaming and complete Brotli channel compression.

mod types;
pub use types::{BrotliLevel, CompressionError};
mod brotli;
pub use brotli::BrotliCompressor;
mod stream;
pub use stream::CompressionStream;
