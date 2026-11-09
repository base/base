//! EIP-4844 blob encoder.

use alloc::{boxed::Box, vec};

use alloy_eips::eip4844::{BYTES_PER_BLOB, Blob, VERSIONED_HASH_VERSION_KZG};

/// Errors returned by [`BlobEncoder::encode`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobEncodeError {
    /// The input data exceeds the maximum blob payload size.
    #[error("data too large: {size} bytes exceeds maximum {max}", max = BlobEncoder::BLOB_MAX_DATA_SIZE)]
    DataTooLarge {
        /// The size of the input data.
        size: usize,
    },
}

/// Encodes raw byte payloads into the Base EIP-4844 blob wire format.
///
/// The encoded blob can be decoded back to the original payload with
/// [`BlobDecoder::decode`](super::BlobDecoder::decode).
#[derive(Debug)]
pub struct BlobEncoder;

impl BlobEncoder {
    /// Blob encoding version used by the Base blob codec.
    pub const BLOB_ENCODING_VERSION: u8 = 0;

    /// Maximum number of data bytes that fit in a single blob.
    ///
    /// Each encoding round packs 4 field elements (128 blob bytes) into 127 data
    /// bytes (4 x 31 payload bytes + 3 reassembled bytes). With 1024 rounds that
    /// gives `127 * 1024 = 130_048` bytes, minus 4 bytes for the version +
    /// 3-byte length header in field element 0.
    pub const BLOB_MAX_DATA_SIZE: usize = (4 * 31 + 3) * Self::BLOB_ENCODING_ROUNDS - 4;

    /// Number of encoding rounds (one per group of 4 field elements).
    pub const BLOB_ENCODING_ROUNDS: usize = 1024;

    /// Encode `data` into a single EIP-4844 [`Blob`].
    ///
    /// Returns a [`BlobEncodeError::DataTooLarge`] if the input exceeds
    /// [`BLOB_MAX_DATA_SIZE`] bytes.
    pub fn encode(data: &[u8]) -> Result<Box<Blob>, BlobEncodeError> {
        if data.len() > Self::BLOB_MAX_DATA_SIZE {
            return Err(BlobEncodeError::DataTooLarge { size: data.len() });
        }

        // The 4 header bytes occupy the first payload slots of field element 0.
        let mut input = vec![0u8; 4 + Self::BLOB_MAX_DATA_SIZE];
        input[4..4 + data.len()].copy_from_slice(data);

        let mut blob = Box::new(Blob::ZERO);
        let out: &mut [u8; BYTES_PER_BLOB] = blob.as_mut();

        // FE0 layout: [high_byte | version | len[1] | len[2] | len[3] | 27 payload bytes]
        //
        // The Base blob encoding stores payload data across field elements.
        // Each field element is 32 bytes: [high_byte | 31 payload bytes].
        // Every 4 field elements the 4 high bytes carry 6-bit chunks that
        // reassemble into 3 additional payload bytes (x, y, z).
        //
        // The decoder produces output in this order per round:
        //   [FE0 payload] [reassembled x] [FE1 payload] [reassembled y]
        //   [FE2 payload] [reassembled z] [FE3 payload]

        for round in 0..Self::BLOB_ENCODING_ROUNDS {
            let ipos = round * 127;
            if ipos >= data.len() + 4 {
                break;
            }
            let pos = round * 128;
            for fe in 0..4 {
                out[pos + fe * 32 + 1..pos + fe * 32 + 32]
                    .copy_from_slice(&input[ipos + fe * 32..ipos + fe * 32 + 31]);
            }
            let (x, y, z) = (input[ipos + 31], input[ipos + 63], input[ipos + 95]);
            out[pos] = x & 0x3F;
            out[pos + 32] = ((x >> 6) & 0x03) << 4 | (y & 0x0F);
            out[pos + 64] = z & 0x3F;
            out[pos + 96] = ((y >> 4) & 0x0F) | (((z >> 6) & 0x03) << 4);
        }

        let len_be = (data.len() as u32).to_be_bytes();
        out[VERSIONED_HASH_VERSION_KZG as usize] = Self::BLOB_ENCODING_VERSION;
        out[2..5].copy_from_slice(&len_be[1..]);

        Ok(blob)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_eips::eip4844::BYTES_PER_BLOB;
    use rstest::rstest;

    use super::{BlobEncodeError, BlobEncoder};
    use crate::BlobDecoder;

    #[rstest]
    #[case::empty(b"")]
    #[case::single_byte(b"x")]
    #[case::hello(b"hello")]
    #[case::zeros(&[0u8; 128])]
    #[case::ones(&[1u8; 1024])]
    #[case::mixed_4k(&(0..=255).cycle().take(4096).collect::<Vec<_>>())]
    #[case::near_max(&[0xAB; BlobEncoder::BLOB_MAX_DATA_SIZE - 1])]
    #[case::exact_max(&[0xCD; BlobEncoder::BLOB_MAX_DATA_SIZE])]
    fn round_trip(#[case] payload: &[u8]) {
        let blob = BlobEncoder::encode(payload).expect("encode should succeed");
        let decoded = BlobDecoder::decode(&blob).expect("decode should succeed");
        assert_eq!(decoded.as_ref(), payload);
    }

    #[test]
    fn data_too_large() {
        let data = vec![0u8; BlobEncoder::BLOB_MAX_DATA_SIZE + 1];
        let err = BlobEncoder::encode(&data).unwrap_err();
        assert_eq!(
            err,
            BlobEncodeError::DataTooLarge { size: BlobEncoder::BLOB_MAX_DATA_SIZE + 1 }
        );
    }

    #[test]
    fn header_layout() {
        let payload = b"hello world";
        let blob = BlobEncoder::encode(payload).unwrap();
        let data: &[u8; BYTES_PER_BLOB] = blob.as_ref();

        // Byte 1 (VERSIONED_HASH_VERSION_KZG offset) contains the encoding version.
        assert_eq!(data[1], 0u8, "encoding version must be 0");

        // Bytes 2..5 contain a 3-byte big-endian length.
        let length = u32::from_be_bytes([0, data[2], data[3], data[4]]) as usize;
        assert_eq!(length, payload.len());
    }
}
