//! Compressed data decoding utilities.

use alloy_rlp::Decodable;

use crate::e2s::error::E2sError;

/// Extension trait for generic decoding from compressed data
pub trait DecodeCompressedRlp {
    /// Decompress and decode the data into the given type
    fn decode<T: Decodable>(&self) -> Result<T, E2sError>;
}
