//! Span batch millisecond timestamp delta primitives.

use alloy_primitives::bytes;
use base_common_consensus::BASE_BLOCK_TIME_MILLIS;

/// A span-batch v2 millisecond timestamp delta between adjacent L2 blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanBatchTimestampDelta {
    /// The millisecond delta from the previous L2 block timestamp.
    pub millis: u64,
}

/// Errors raised while validating or decoding span-batch millisecond timestamp deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpanBatchTimestampDeltaError {
    /// The encoded delta was zero, which would not advance the L2 timestamp.
    #[error("span batch timestamp delta must be non-zero")]
    Zero,
    /// The encoded delta was not aligned to Base's 200ms slot cadence.
    #[error("span batch timestamp delta {0}ms is not 200ms aligned")]
    NotSlotAligned(u64),
    /// The encoded varint could not be decoded.
    #[error("failed to decode span batch timestamp delta varint")]
    Decoding,
}

impl SpanBatchTimestampDelta {
    /// Creates a validated span-batch timestamp delta.
    pub const fn new(millis: u64) -> Result<Self, SpanBatchTimestampDeltaError> {
        if millis == 0 {
            return Err(SpanBatchTimestampDeltaError::Zero);
        }
        if !millis.is_multiple_of(BASE_BLOCK_TIME_MILLIS as u64) {
            return Err(SpanBatchTimestampDeltaError::NotSlotAligned(millis));
        }
        Ok(Self { millis })
    }

    /// Encodes the timestamp delta as an unsigned varint.
    pub fn encode(&self, out: &mut dyn bytes::BufMut) {
        let mut buf = [0u8; 10];
        out.put_slice(unsigned_varint::encode::u64(self.millis, &mut buf));
    }

    /// Decodes and validates a timestamp delta from an unsigned varint.
    pub fn decode(input: &mut &[u8]) -> Result<Self, SpanBatchTimestampDeltaError> {
        let (millis, remaining) = unsigned_varint::decode::u64(input)
            .map_err(|_| SpanBatchTimestampDeltaError::Decoding)?;
        *input = remaining;
        Self::new(millis)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn timestamp_delta_accepts_200ms_multiples() {
        assert_eq!(SpanBatchTimestampDelta::new(200).unwrap().millis, 200);
        assert_eq!(SpanBatchTimestampDelta::new(1_000).unwrap().millis, 1_000);
    }

    #[test]
    fn timestamp_delta_rejects_zero_or_unaligned_values() {
        assert_eq!(SpanBatchTimestampDelta::new(0), Err(SpanBatchTimestampDeltaError::Zero));
        assert_eq!(
            SpanBatchTimestampDelta::new(100),
            Err(SpanBatchTimestampDeltaError::NotSlotAligned(100))
        );
    }

    #[test]
    fn timestamp_delta_varint_roundtrip() {
        let delta = SpanBatchTimestampDelta::new(600).unwrap();
        let mut encoded = Vec::new();
        delta.encode(&mut encoded);

        let mut input = encoded.as_slice();
        assert_eq!(SpanBatchTimestampDelta::decode(&mut input).unwrap(), delta);
        assert!(input.is_empty());
    }
}
