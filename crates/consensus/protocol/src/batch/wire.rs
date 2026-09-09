//! Tagged wire encoding for singular batches.

use alloy_rlp::{Buf, BufMut, Decodable, Encodable};

use crate::{BatchDecodingError, SingleBatch};

impl SingleBatch {
    /// The singular batch wire discriminator.
    pub const TYPE: u8 = 0;

    /// Decodes a tagged singular batch, rejecting unsupported batch formats.
    pub fn decode_batch(reader: &mut &[u8]) -> Result<Self, BatchDecodingError> {
        let tag = *reader.first().ok_or(BatchDecodingError::EmptyBuffer)?;
        if tag != Self::TYPE {
            return Err(BatchDecodingError::InvalidBatchType(tag));
        }
        reader.advance(1);
        Self::decode(reader).map_err(BatchDecodingError::AlloyRlpError)
    }

    /// Encodes the discriminator followed by the batch's RLP fields.
    pub fn encode_batch(&self, out: &mut dyn BufMut) {
        out.put_u8(Self::TYPE);
        self.encode(out);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn singular_wire_roundtrip() {
        let batch = SingleBatch { epoch_num: 42, timestamp: 1234, ..Default::default() };
        let mut bytes = Vec::new();
        batch.encode_batch(&mut bytes);
        let mut reader = bytes.as_slice();
        assert_eq!(SingleBatch::decode_batch(&mut reader).unwrap(), batch);
        assert!(reader.is_empty());
    }

    #[test]
    fn rejects_span_and_unknown_discriminators() {
        for tag in [1, 2, 255] {
            assert_eq!(
                SingleBatch::decode_batch(&mut [tag].as_slice()),
                Err(BatchDecodingError::InvalidBatchType(tag))
            );
        }
        assert_eq!(
            SingleBatch::decode_batch(&mut [].as_slice()),
            Err(BatchDecodingError::EmptyBuffer)
        );
    }
}
