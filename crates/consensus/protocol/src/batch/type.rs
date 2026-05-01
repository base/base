//! Batch Types
//!
//! This module contains the batch types for the Base derivation pipeline.
//!
//! ## Batch
//!
//! A batch is either a `SpanBatch` or a `SingleBatch`.
//!
//! The batch type is encoded as a single byte:
//! - `0x00` for a `SingleBatch`
//! - `0x01` for a `SpanBatch`

use alloy_rlp::{Decodable, Encodable};

use crate::BatchDecodingError;

/// The Batch Type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum BatchType {
    /// Single Batch.
    #[default]
    Single = Self::SINGLE,
    /// Span Batch.
    Span = Self::SPAN,
}

impl BatchType {
    /// The single batch type identifier.
    pub const SINGLE: u8 = 0x00;

    /// The span batch type identifier.
    pub const SPAN: u8 = 0x01;
}

impl TryFrom<u8> for BatchType {
    type Error = BatchDecodingError;
    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            Self::SINGLE => Ok(Self::Single),
            Self::SPAN => Ok(Self::Span),
            _ => Err(BatchDecodingError::InvalidBatchType(val)),
        }
    }
}

impl Encodable for BatchType {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        (*self as u8).encode(out);
    }
}

impl Decodable for BatchType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let val = u8::decode(buf)?;
        Self::try_from(val).map_err(|_| alloy_rlp::Error::Custom("invalid batch type"))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn test_batch_type_rlp_roundtrip() {
        for batch_type in [BatchType::Single, BatchType::Span] {
            let mut buf = Vec::new();
            batch_type.encode(&mut buf);
            let decoded = BatchType::decode(&mut buf.as_slice()).unwrap();
            assert_eq!(batch_type, decoded);
        }
    }

    #[test]
    fn test_invalid_batch_type() {
        let result = BatchType::decode(&mut [2u8].as_slice());
        assert!(result.is_err());
    }
}
