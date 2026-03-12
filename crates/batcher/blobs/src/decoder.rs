//! EIP-4844 blob decoder.

use alloy_eips::eip4844::{BYTES_PER_BLOB, Blob, VERSIONED_HASH_VERSION_KZG};
use alloy_primitives::Bytes;

use crate::encoder::BLOB_MAX_DATA_SIZE;

/// Number of encoding rounds (one per group of 4 field elements).
const BLOB_ENCODING_ROUNDS: usize = 1024;

/// Errors returned by [`BlobDecoder::decode`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobDecodeError {
    /// A field element has one of its two high-order bits set.
    #[error("invalid field element at byte offset {offset}")]
    InvalidFieldElement {
        /// The byte offset within the blob where the invalid field element
        /// was encountered.
        offset: usize,
    },
    /// The encoding version byte is not the expected value.
    #[error("invalid encoding version: expected 0, got {version}")]
    InvalidEncodingVersion {
        /// The version byte that was encountered.
        version: u8,
    },
    /// The declared payload length exceeds the maximum allowed size.
    #[error("invalid length: {length} exceeds maximum {max}", max = BLOB_MAX_DATA_SIZE)]
    InvalidLength {
        /// The declared length.
        length: usize,
    },
    /// Trailing non-zero bytes found after the declared payload in the output
    /// or in unused blob space.
    #[error("non-zero padding at byte offset {offset}")]
    NonZeroPadding {
        /// The byte offset where non-zero padding was found.
        offset: usize,
    },
}

/// Decodes Base EIP-4844 blobs back to raw byte payloads.
///
/// This is the inverse of
/// [`BlobEncoder::encode`](super::BlobEncoder::encode) and mirrors the
/// decoding logic from `BlobData::decode()` in `base-consensus-derive`.
#[derive(Debug)]
pub struct BlobDecoder;

impl BlobDecoder {
    /// Decode a [`Blob`] back to the raw payload bytes.
    ///
    /// Returns a [`BlobDecodeError`] if the blob has an invalid encoding
    /// version, length, field element, or non-zero trailing padding.
    pub fn decode(blob: &Blob) -> Result<Bytes, BlobDecodeError> {
        let data: &[u8; BYTES_PER_BLOB] = blob.as_ref();

        // Validate encoding version.
        let version = data[VERSIONED_HASH_VERSION_KZG as usize];
        if version != 0 {
            return Err(BlobDecodeError::InvalidEncodingVersion { version });
        }

        // Decode 3-byte big-endian length.
        let length = u32::from_be_bytes([0, data[2], data[3], data[4]]) as usize;
        if length > BLOB_MAX_DATA_SIZE {
            return Err(BlobDecodeError::InvalidLength { length });
        }

        let mut output = vec![0u8; BLOB_MAX_DATA_SIZE];

        // --- Round 0 ---
        // FE0 payload: 27 bytes from data[5..32].
        output[0..27].copy_from_slice(&data[5..32]);

        let mut output_pos = 28usize;
        let mut input_pos = 32usize;
        let mut encoded_byte = [0u8; 4];
        encoded_byte[0] = data[0];

        // Decode field elements 1..3, validating high-order bits.
        for b in encoded_byte.iter_mut().skip(1) {
            let (enc, opos, ipos) =
                Self::decode_field_element(data, output_pos, input_pos, &mut output)?;
            *b = enc;
            output_pos = opos;
            input_pos = ipos;
        }

        // Reassemble the 4 x 6-bit encoded chunks into 3 bytes of output.
        output_pos = Self::reassemble_bytes(output_pos, &encoded_byte, &mut output);

        // --- Rounds 1..1024 ---
        for _ in 1..BLOB_ENCODING_ROUNDS {
            if output_pos >= length {
                break;
            }

            for d in &mut encoded_byte {
                let (enc, opos, ipos) =
                    Self::decode_field_element(data, output_pos, input_pos, &mut output)?;
                *d = enc;
                output_pos = opos;
                input_pos = ipos;
            }
            output_pos = Self::reassemble_bytes(output_pos, &encoded_byte, &mut output);
        }

        // Validate that all output bytes past the declared length are zero.
        for (i, byte) in output.iter().enumerate().skip(length) {
            if *byte != 0 {
                return Err(BlobDecodeError::NonZeroPadding { offset: i });
            }
        }

        // Validate that all remaining input bytes are zero.
        for (i, byte) in data.iter().enumerate().skip(input_pos) {
            if *byte != 0 {
                return Err(BlobDecodeError::NonZeroPadding { offset: i });
            }
        }

        output.truncate(length);
        Ok(Bytes::from(output))
    }

    /// Decode a single field element: validate that the two high-order bits
    /// are clear, copy the 31 payload bytes to `output`, and return the
    /// encoded high byte along with updated positions.
    fn decode_field_element(
        data: &[u8; BYTES_PER_BLOB],
        output_pos: usize,
        input_pos: usize,
        output: &mut [u8],
    ) -> Result<(u8, usize, usize), BlobDecodeError> {
        if data[input_pos] & 0b1100_0000 != 0 {
            return Err(BlobDecodeError::InvalidFieldElement { offset: input_pos });
        }
        output[output_pos..output_pos + 31].copy_from_slice(&data[input_pos + 1..input_pos + 32]);
        Ok((data[input_pos], output_pos + 32, input_pos + 32))
    }

    /// Reassemble 4 x 6-bit encoded chunks into 3 bytes of output (x, y, z)
    /// and place them in their correct positions.
    fn reassemble_bytes(mut output_pos: usize, encoded_byte: &[u8], output: &mut [u8]) -> usize {
        output_pos -= 1;
        let x = (encoded_byte[0] & 0b0011_1111) | ((encoded_byte[1] & 0b0011_0000) << 2);
        let y = (encoded_byte[1] & 0b0000_1111) | ((encoded_byte[3] & 0b0000_1111) << 4);
        let z = (encoded_byte[2] & 0b0011_1111) | ((encoded_byte[3] & 0b0011_0000) << 2);
        output[output_pos - 32] = z;
        output[output_pos - (32 * 2)] = y;
        output[output_pos - (32 * 3)] = x;
        output_pos
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip4844::{BYTES_PER_BLOB, Blob, VERSIONED_HASH_VERSION_KZG};
    use rstest::rstest;

    use super::{BlobDecodeError, BlobDecoder};
    use crate::BlobEncoder;

    #[rstest]
    #[case::single_byte(b"a")]
    #[case::short_message(b"hello world")]
    #[case::binary_256(&(0..=255).collect::<Vec<_>>())]
    #[case::kilobyte(&[0xFFu8; 1024])]
    fn decode_round_trip(#[case] payload: &[u8]) {
        let blob = BlobEncoder::encode(payload).expect("encode");
        let decoded = BlobDecoder::decode(&blob).expect("decode");
        assert_eq!(decoded.as_ref(), payload);
    }

    #[test]
    fn invalid_encoding_version() {
        let mut blob = Box::new(Blob::ZERO);
        let data: &mut [u8; BYTES_PER_BLOB] = blob.as_mut();
        // Set version to 1 instead of 0.
        data[VERSIONED_HASH_VERSION_KZG as usize] = 1;
        let err = BlobDecoder::decode(&blob).unwrap_err();
        assert_eq!(err, BlobDecodeError::InvalidEncodingVersion { version: 1 });
    }

    #[test]
    fn invalid_length() {
        let mut blob = Box::new(Blob::ZERO);
        let data: &mut [u8; BYTES_PER_BLOB] = blob.as_mut();
        data[VERSIONED_HASH_VERSION_KZG as usize] = 0;
        // Set length to 0xFF_FF_FF which exceeds BLOB_MAX_DATA_SIZE.
        data[2] = 0xFF;
        data[3] = 0xFF;
        data[4] = 0xFF;
        let err = BlobDecoder::decode(&blob).unwrap_err();
        assert!(matches!(err, BlobDecodeError::InvalidLength { .. }));
    }

    #[test]
    fn invalid_field_element() {
        let mut blob = Box::new(Blob::ZERO);
        let data: &mut [u8; BYTES_PER_BLOB] = blob.as_mut();
        data[VERSIONED_HASH_VERSION_KZG as usize] = 0;
        // Set a valid short length.
        data[4] = 1;
        // Set high bits on field element 1 (byte offset 32).
        data[32] = 0b1100_0000;
        let err = BlobDecoder::decode(&blob).unwrap_err();
        assert_eq!(err, BlobDecodeError::InvalidFieldElement { offset: 32 });
    }

    #[test]
    fn zero_length_payload() {
        let blob = BlobEncoder::encode(b"").expect("encode empty");
        let decoded = BlobDecoder::decode(&blob).expect("decode empty");
        assert!(decoded.is_empty());
    }
}
