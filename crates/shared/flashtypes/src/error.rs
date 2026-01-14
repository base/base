//! Contains error types relating to primitive flashblock type functionality.

use derive_more::{Display, Error};

/// Errors that can occur while decoding a flashblock payload.
#[derive(Debug, Display, Error)]
pub enum FlashblockDecodeError {
    /// Failed to deserialize the flashblock payload JSON into the expected struct.
    #[display("failed to parse flashblock payload JSON: {_0}")]
    PayloadParse(serde_json::Error),
    /// Failed to deserialize the flashblock metadata into the expected struct.
    #[display("failed to parse flashblock metadata: {_0}")]
    MetadataParse(serde_json::Error),
    /// Brotli decompression failed.
    #[display("failed to decompress brotli payload: {_0}")]
    Decompress(std::io::Error),
    /// The decompressed payload was not valid UTF-8 JSON.
    #[display("decompressed payload is not valid UTF-8 JSON: {_0}")]
    Utf8(std::string::FromUtf8Error),
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde::de::Error as _;

    use super::*;

    #[rstest]
    #[case::payload_parse(FlashblockDecodeError::PayloadParse(serde_json::Error::custom("test")))]
    #[case::metadata_parse(FlashblockDecodeError::MetadataParse(serde_json::Error::custom(
        "test"
    )))]
    #[case::decompress(FlashblockDecodeError::Decompress(std::io::Error::other("test")))]
    #[case::utf8(FlashblockDecodeError::Utf8(String::from_utf8(vec![0xff, 0xfe]).unwrap_err()))]
    fn test_flashblock_decode_error_display(#[case] error: FlashblockDecodeError) {
        let display = format!("{}", error);
        assert!(!display.is_empty());
    }
}
