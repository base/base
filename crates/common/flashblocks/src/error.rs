//! Contains error types relating to primitive flashblock type functionality.

use thiserror::Error;

/// Errors that can occur while decoding a flashblock payload.
#[derive(Debug, Error)]
pub enum FlashblockDecodeError {
    /// Failed to deserialize the flashblock payload JSON into the expected struct.
    #[error("failed to parse flashblock payload JSON: {0}")]
    PayloadParse(#[source] serde_json::Error),
    /// Failed to deserialize the flashblock metadata into the expected struct.
    #[error("failed to parse flashblock metadata: {0}")]
    MetadataParse(#[source] serde_json::Error),
    /// Brotli decompression failed.
    #[error("failed to decompress brotli payload: {0}")]
    Decompress(#[source] std::io::Error),
    /// The decoded payload exceeded the maximum allowed size.
    #[error("flashblock payload too large: {given} bytes, max {max}")]
    PayloadTooLarge {
        /// Decoded payload size.
        given: usize,
        /// Maximum allowed decoded payload size.
        max: usize,
    },
    /// The decompressed payload was not valid UTF-8 JSON.
    #[error("decompressed payload is not valid UTF-8 JSON: {0}")]
    Utf8(#[source] std::string::FromUtf8Error),
}
