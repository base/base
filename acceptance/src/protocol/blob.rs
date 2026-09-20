//! Hash-authenticated beacon blobs decoded with the production derivation codec.

use alloy_eips::eip4844::Blob;
use alloy_primitives::{B256, Bytes};
use base_consensus_derive::BlobData;
use base_consensus_providers::{BeaconClient, OnlineBeaconClient};
use eyre::{Result, ensure};

/// Exact blob bytes and the frame payload recovered from an L1 transaction's versioned hash.
#[derive(Clone, Debug)]
pub struct BlobEvidence {
    /// Versioned hash included by the actual L1 EIP-4844 transaction.
    pub versioned_hash: B256,
    /// Payload produced by the production OP blob decoder.
    pub payload: Bytes,
}

impl BlobEvidence {
    /// Requests only the transaction's blobs; the production client recomputes their KZG hashes.
    pub async fn fetch(beacon: &str, slot: u64, hashes: &[B256]) -> Result<Vec<Self>> {
        ensure!(!hashes.is_empty(), "blob batch transaction has no versioned hashes");
        let blobs = OnlineBeaconClient::new_http(beacon.to_owned())
            .filtered_beacon_blobs(slot, hashes)
            .await?;
        ensure!(blobs.len() == hashes.len(), "beacon returned incomplete batch blob data");
        hashes.iter().zip(blobs).map(|(hash, blob)| Self::decode(*hash, &blob.blob)).collect()
    }

    /// Decodes a fixed-size blob after its association with the L1 hash has been verified.
    pub fn decode(versioned_hash: B256, blob: &Blob) -> Result<Self> {
        let payload =
            BlobData { data: Some(Bytes::copy_from_slice(blob.as_slice())), calldata: None }
                .decode()?;
        Ok(Self { versioned_hash, payload })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use base_batcher_encoder::FrameEncoder;
    use base_blobs::BlobEncoder;
    use base_protocol::Frame;

    use super::BlobEvidence;

    #[test]
    fn recovers_real_op_encoded_frames_from_a_blob() {
        let frame = Frame::new([3; 16], 0, vec![7, 8, 9], true);
        let payload = FrameEncoder::to_calldata(&frame);
        let blob = BlobEncoder::encode(&payload).unwrap();
        let evidence = BlobEvidence::decode(B256::repeat_byte(1), &blob).unwrap();
        assert_eq!(Frame::parse_frames(&evidence.payload).unwrap(), vec![frame]);
        assert_eq!(evidence.versioned_hash, B256::repeat_byte(1));
    }

    #[test]
    fn rejects_malformed_blob_encoding() {
        let mut blob = BlobEncoder::encode(&[0, 1, 2]).unwrap();
        blob[1] = 0xff;
        assert!(BlobEvidence::decode(B256::ZERO, &blob).is_err());
    }
}
