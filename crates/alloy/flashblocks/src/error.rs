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
    /// The decompressed payload was not valid UTF-8 JSON.
    #[error("decompressed payload is not valid UTF-8 JSON: {0}")]
    Utf8(#[source] std::string::FromUtf8Error),
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde::de::Error as _;

    use super::FlashblockDecodeError;

    #[rstest]
    #[case::payload_parse(FlashblockDecodeError::PayloadParse(serde_json::Error::custom("test")))]
    #[case::metadata_parse(FlashblockDecodeError::MetadataParse(serde_json::Error::custom(
        "test"
    )))]
    #[case::decompress(FlashblockDecodeError::Decompress(std::io::Error::other("test")))]
    #[case::utf8(FlashblockDecodeError::Utf8(String::from_utf8(vec![0xff, 0xfe]).unwrap_err()))]
    fn test_flashblock_decode_error_display(#[case] error: FlashblockDecodeError) {
        let display = format!("{error}");
        assert!(!display.is_empty());
    }
}
