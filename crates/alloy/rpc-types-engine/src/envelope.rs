//! Optimism execution payload envelope in network format and related types.
//!
//! This module uses the `snappy` compression algorithm to decompress the payload.
//! The license for snappy can be found in the `SNAPPY-LICENSE` at the root of the repository.

use alloy_primitives::{keccak256, Signature, B256};
use alloy_rpc_types_engine::ExecutionPayload;
use derive_more::derive::{Display, From};

/// Optimism execution payload envelope in network format.
///
/// This struct is used to represent payloads that are sent over the Optimism
/// CL p2p network in a snappy-compressed format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpNetworkPayloadEnvelope {
    /// The execution payload.
    pub payload: ExecutionPayload,
    /// A signature for the payload.
    pub signature: Signature,
    /// The hash of the payload.
    pub payload_hash: PayloadHash,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
}

impl OpNetworkPayloadEnvelope {
    /// Decode a payload envelope from a snappy-compressed byte array.
    /// The payload version decoded is `ExecutionPayloadV1` from SSZ bytes.
    #[cfg(feature = "std")]
    pub fn decode_v1(data: &[u8]) -> Result<Self, PayloadEnvelopeError> {
        use ssz::Decode;
        let mut decoder = snap::raw::Decoder::new();
        let decompressed = decoder.decompress_vec(data)?;

        if decompressed.len() < 66 {
            return Err(PayloadEnvelopeError::InvalidLength);
        }

        let sig_data = &decompressed[..65];
        let block_data = &decompressed[65..];

        let signature = Signature::try_from(sig_data)?;
        let hash = PayloadHash::from(block_data);

        let payload = ExecutionPayload::from(
            alloy_rpc_types_engine::ExecutionPayloadV1::from_ssz_bytes(block_data)?,
        );

        Ok(Self { payload, signature, payload_hash: hash, parent_beacon_block_root: None })
    }

    /// Decode a payload envelope from a snappy-compressed byte array.
    /// The payload version decoded is `ExecutionPayloadV2` from SSZ bytes.
    #[cfg(feature = "std")]
    pub fn decode_v2(data: &[u8]) -> Result<Self, PayloadEnvelopeError> {
        use ssz::Decode;
        let mut decoder = snap::raw::Decoder::new();
        let decompressed = decoder.decompress_vec(data)?;

        if decompressed.len() < 66 {
            return Err(PayloadEnvelopeError::InvalidLength);
        }

        let sig_data = &decompressed[..65];
        let block_data = &decompressed[65..];

        let signature = Signature::try_from(sig_data)?;
        let hash = PayloadHash::from(block_data);

        let payload = ExecutionPayload::from(
            alloy_rpc_types_engine::ExecutionPayloadV2::from_ssz_bytes(block_data)?,
        );

        Ok(Self { payload, signature, payload_hash: hash, parent_beacon_block_root: None })
    }

    /// Decode a payload envelope from a snappy-compressed byte array.
    /// The payload version decoded is `ExecutionPayloadV3` from SSZ bytes.
    #[cfg(feature = "std")]
    pub fn decode_v3(data: &[u8]) -> Result<Self, PayloadEnvelopeError> {
        use ssz::Decode;
        let mut decoder = snap::raw::Decoder::new();
        let decompressed = decoder.decompress_vec(data)?;

        if decompressed.len() < 98 {
            return Err(PayloadEnvelopeError::InvalidLength);
        }

        let sig_data = &decompressed[..65];
        let parent_beacon_block_root = &decompressed[65..97];
        let block_data = &decompressed[97..];

        let signature = Signature::try_from(sig_data)?;
        let parent_beacon_block_root = Some(B256::from_slice(parent_beacon_block_root));
        let hash = PayloadHash::from(block_data);

        let payload = ExecutionPayload::from(
            alloy_rpc_types_engine::ExecutionPayloadV3::from_ssz_bytes(block_data)?,
        );

        Ok(Self { payload, signature, payload_hash: hash, parent_beacon_block_root })
    }
}

/// Errors that can occur when decoding a payload envelope.
#[derive(Debug, Clone, PartialEq, Eq, Display, From)]
pub enum PayloadEnvelopeError {
    /// The snappy encoding is broken.
    #[display("Broken snappy encoding")]
    #[cfg_attr(feature = "std", from(snap::Error))]
    BrokenSnappyEncoding,
    /// The signature is invalid.
    #[display("Invalid signature")]
    #[from(alloy_primitives::SignatureError)]
    InvalidSignature,
    /// The SSZ encoding is broken.
    #[display("Broken SSZ encoding")]
    #[cfg_attr(feature = "std", from(ssz::DecodeError))]
    BrokenSszEncoding,
    /// The payload envelope is of invalid length.
    #[display("Invalid length")]
    InvalidLength,
}

/// Represents the Keccak256 hash of the block
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
pub struct PayloadHash(pub B256);

impl From<&[u8]> for PayloadHash {
    /// Returns the Keccak256 hash of a sequence of bytes
    fn from(value: &[u8]) -> Self {
        Self(keccak256(value))
    }
}

impl PayloadHash {
    /// The expected message that should be signed by the unsafe block signer.
    pub fn signature_message(&self, chain_id: u64) -> B256 {
        let domain = B256::ZERO.as_slice();
        let chain_id = B256::left_padding_from(&chain_id.to_be_bytes()[..]);
        let payload_hash = self.0.as_slice();
        keccak256([domain, chain_id.as_slice(), payload_hash].concat())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn test_signature_message() {
        let inner = b256!("9999999999999999999999999999999999999999999999999999999999999999");
        let hash = PayloadHash::from(inner.as_slice());
        let chain_id = 10;
        let expected = b256!("44a0e2b1aba1aae1771eddae1dcd2ad18a8cdac8891517153f03253e49d3f206");
        assert_eq!(hash.signature_message(chain_id), expected);
    }

    #[test]
    fn test_inner_payload_hash() {
        arbtest::arbtest(|u| {
            let inner = B256::from(u.arbitrary::<[u8; 32]>()?);
            let hash = PayloadHash::from(inner.as_slice());
            assert_eq!(hash.0, keccak256(inner.as_slice()));
            Ok(())
        });
    }

    #[test]
    #[cfg(feature = "std")]
    fn decode_payload_v1() {
        use alloy_primitives::hex;
        let data = hex::decode("0xbd04f043128457c6ccf35128497167442bcc0f8cce78cda8b366e6a12e526d938d1e4c1046acffffbfc542a7e212bb7d80d3a4b2f84f7b196d935398a24eb84c519789b401000000fe0300fe0300fe0300fe0300fe0300fe0300a203000c4a8fd56621ad04fc0101067601008ce60be0005b220117c32c0f3b394b346c2aa42cfa8157cd41f891aa0bec485a62fc010000").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v1(&data).unwrap();
        assert_eq!(1725271882, payload_envelop.payload.timestamp());
    }

    #[test]
    #[cfg(feature = "std")]
    fn decode_payload_v2() {
        use alloy_primitives::hex;
        let data = hex::decode("0xc104f0433805080eb36c0b130a7cc1dc74c3f721af4e249aa6f61bb89d1557143e971bb738a3f3b98df7c457e74048e9d2d7e5cd82bb45e3760467e2270e9db86d1271a700000000fe0300fe0300fe0300fe0300fe0300fe0300a203000c6b89d46525ad000205067201009cda69cb5b9b73fc4eb2458b37d37f04ff507fe6c9cd2ab704a05ea9dae3cd61760002000000020000").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v2(&data).unwrap();
        assert_eq!(1708427627, payload_envelop.payload.timestamp());
    }

    #[test]
    #[cfg(feature = "std")]
    fn decode_payload_v3() {
        use alloy_primitives::hex;
        let data = hex::decode("0xf104f0434442b9eb38b259f5b23826e6b623e829d2fb878dac70187a1aecf42a3f9bedfd29793d1fcb5822324be0d3e12340a95855553a65d64b83e5579dffb31470df5d010000006a03000412346a1d00fe0100fe0100fe0100fe0100fe0100fe01004201000cc588d465219504100201067601007cfece77b89685f60e3663b6e0faf2de0734674eb91339700c4858c773a8ff921e014401043e0100").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v3(&data).unwrap();
        assert_eq!(1708427461, payload_envelop.payload.timestamp());
    }
}
