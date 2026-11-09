//! Contains the `BlobData` struct.

use alloc::boxed::Box;

use alloy_eips::eip4844::Blob;
use alloy_primitives::Bytes;
use base_common_blobs::BlobDecoder;
pub use base_protocol::BLOB_MAX_DATA_SIZE;

use crate::{BlobDecodingError, BlobProviderError};

/// The Blob Data
#[derive(Default, Clone, Debug)]
pub struct BlobData {
    /// The blob data
    pub data: Option<Bytes>,
    /// The calldata
    pub calldata: Option<Bytes>,
}

impl BlobData {
    /// Decodes the blob into raw byte data.
    /// Returns a [`BlobDecodingError`] if the blob is invalid.
    pub fn decode(&self) -> Result<Bytes, BlobDecodingError> {
        let data = self.data.as_ref().ok_or(BlobDecodingError::MissingData)?;
        let blob =
            <&Blob>::try_from(data.as_ref()).map_err(|_| BlobDecodingError::InvalidLength)?;
        Ok(BlobDecoder::decode(blob)?)
    }

    /// Fills in the pointers to the fetched blob bodies.
    /// There should be exactly one placeholder blobOrCalldata
    /// element for each blob, otherwise an error is returned.
    pub fn fill(&mut self, blobs: &[Box<Blob>], index: usize) -> Result<bool, BlobProviderError> {
        // Do not fill if there is calldata here
        if self.calldata.is_some() {
            return Ok(false);
        }

        if index >= blobs.len() {
            return Err(BlobProviderError::NotEnoughBlobs(index, blobs.len()));
        }

        if blobs[index].is_empty() {
            return Err(BlobProviderError::BlobDecoding(BlobDecodingError::MissingData));
        }

        self.data = Some(Bytes::from(*blobs[index]));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_eips::eip4844::{BYTES_PER_BLOB, VERSIONED_HASH_VERSION_KZG};

    use super::*;

    #[test]
    fn test_cannot_fill_empty_calldata() {
        let mut blob_data = BlobData { calldata: Some(Bytes::new()), ..Default::default() };
        let blobs = vec![Box::new(Blob::with_last_byte(1u8))];
        assert_eq!(blob_data.fill(&blobs, 0), Ok(false));
    }

    #[test]
    fn test_fill_oob_index() {
        let mut blob_data = BlobData::default();
        let blobs = vec![Box::new(Blob::with_last_byte(1u8))];
        assert_eq!(blob_data.fill(&blobs, 1), Err(BlobProviderError::NotEnoughBlobs(1, 1)));
    }

    #[test]
    fn test_fill_zero_blob() {
        let mut blob_data = BlobData::default();
        let blobs = vec![Box::new(Blob::ZERO)];
        // consider a blob made entirely of zero bytes a regular blob
        assert_eq!(blob_data.fill(&blobs, 0), Ok(true));
    }

    #[test]
    fn test_fill_blob() {
        let mut blob_data = BlobData::default();
        let blobs = vec![Box::new(Blob::with_last_byte(1u8))];
        assert_eq!(blob_data.fill(&blobs, 0), Ok(true));
        let expected = Bytes::from([&[0u8; 131071][..], &[1u8]].concat());
        assert_eq!(blob_data.data, Some(expected));
    }

    #[test]
    fn test_blob_data_decode_missing_data() {
        let blob_data = BlobData::default();
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::MissingData));
    }

    #[test]
    fn test_blob_data_decode_wrong_size() {
        let blob_data = BlobData { data: Some(Bytes::from(vec![0u8; 32])), ..Default::default() };
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::InvalidLength));
    }

    #[test]
    fn test_blob_data_decode_invalid_encoding_version() {
        let mut data = vec![0u8; BYTES_PER_BLOB];
        data[VERSIONED_HASH_VERSION_KZG as usize] = 1;
        let blob_data = BlobData { data: Some(Bytes::from(data)), ..Default::default() };
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::InvalidEncodingVersion));
    }

    #[test]
    fn test_blob_data_decode_invalid_length() {
        let mut data = vec![0u8; BYTES_PER_BLOB];
        data[2] = 0xFF;
        data[3] = 0xFF;
        data[4] = 0xFF;
        let blob_data = BlobData { data: Some(Bytes::from(data)), ..Default::default() };
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::InvalidLength));
    }

    #[test]
    fn test_blob_data_decode() {
        let mut data = vec![0u8; BYTES_PER_BLOB];
        data[4] = 0x01;
        let blob_data = BlobData { data: Some(Bytes::from(data)), ..Default::default() };
        assert_eq!(blob_data.decode(), Ok(Bytes::from(vec![0u8; 1])));
    }

    #[test]
    fn test_blob_data_decode_non_zero_padding() {
        let mut data = vec![0u8; BYTES_PER_BLOB];
        data[4] = 0x01;
        data[33] = 0x01;
        let blob_data = BlobData { data: Some(Bytes::from(data)), ..Default::default() };
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::InvalidFieldElement));
    }

    #[test]
    fn test_blob_data_decode_invalid_field_element() {
        let mut data = vec![0u8; BYTES_PER_BLOB];
        data[4] = 0x01;
        data[32] = 0b1100_0000;
        let blob_data = BlobData { data: Some(Bytes::from(data)), ..Default::default() };
        assert_eq!(blob_data.decode(), Err(BlobDecodingError::InvalidFieldElement));
    }
}
