//! Optimism execution payload envelope in network format and related types.
//!
//! This module uses the `snappy` compression algorithm to decompress the payload.
//! The license for snappy can be found in the `SNAPPY-LICENSE` at the root of the repository.

use crate::{OpExecutionPayload, OpExecutionPayloadSidecar, OpExecutionPayloadV4};
use alloc::vec::Vec;
use alloy_consensus::{Block, BlockHeader, Sealable, Transaction};
use alloy_eips::{Encodable2718, eip4895::Withdrawal, eip7685::Requests};
use alloy_primitives::{B256, Signature, keccak256};
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayloadInputV2, ExecutionPayloadV3, PraguePayloadFields,
};

/// A thin wrapper around [`OpExecutionPayload`] that includes the parent beacon block root.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OpExecutionPayloadEnvelope {
    /// The execution payload.
    pub payload: OpExecutionPayload,
    /// The parent beacon block root, if any.
    pub parent_beacon_block_root: Option<B256>,
}

impl From<OpNetworkPayloadEnvelope> for OpExecutionPayloadEnvelope {
    fn from(envelope: OpNetworkPayloadEnvelope) -> Self {
        Self {
            payload: envelope.payload,
            parent_beacon_block_root: envelope.parent_beacon_block_root,
        }
    }
}

/// Struct aggregating [`OpExecutionPayload`] and [`OpExecutionPayloadSidecar`] and encapsulating
/// complete payload supplied for execution.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpExecutionData {
    /// Execution payload.
    pub payload: OpExecutionPayload,
    /// Additional fork-specific fields.
    pub sidecar: OpExecutionPayloadSidecar,
}

impl OpExecutionData {
    /// Creates new instance of [`OpExecutionData`].
    pub const fn new(payload: OpExecutionPayload, sidecar: OpExecutionPayloadSidecar) -> Self {
        Self { payload, sidecar }
    }

    /// Conversion from [`alloy_consensus::Block`]. Also returns the [`OpExecutionPayloadSidecar`]
    /// extracted from the block.
    ///
    /// See also [`from_block_unchecked`](OpExecutionPayload::from_block_slow).
    ///
    /// Note: This re-calculates the block hash.
    pub fn from_block_slow<T, H>(block: &Block<T, H>) -> Self
    where
        T: Encodable2718 + Transaction,
        H: BlockHeader + Sealable,
    {
        let (payload, sidecar) = OpExecutionPayload::from_block_slow(block);

        Self::new(payload, sidecar)
    }

    /// Conversion from [`alloy_consensus::Block`]. Also returns the [`OpExecutionPayloadSidecar`]
    /// extracted from the block.
    ///
    /// See also [`OpExecutionPayload::from_block_unchecked`].
    pub fn from_block_unchecked<T, H>(block_hash: B256, block: &Block<T, H>) -> Self
    where
        T: Encodable2718 + Transaction,
        H: BlockHeader,
    {
        let (payload, sidecar) = OpExecutionPayload::from_block_unchecked(block_hash, block);

        Self::new(payload, sidecar)
    }

    /// Creates a new instance from args to engine API method `newPayloadV2`.
    ///
    /// Spec: <https://specs.optimism.io/protocol/exec-engine.html#engine_newpayloadv2>
    pub fn v2(payload: ExecutionPayloadInputV2) -> Self {
        Self::new(OpExecutionPayload::v2(payload), OpExecutionPayloadSidecar::default())
    }

    /// Creates a new instance from args to engine API method `newPayloadV3`.
    ///
    /// Spec: <https://specs.optimism.io/protocol/exec-engine.html#engine_newpayloadv3>
    pub fn v3(
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Self {
        Self::new(
            OpExecutionPayload::v3(payload),
            OpExecutionPayloadSidecar::v3(CancunPayloadFields::new(
                parent_beacon_block_root,
                versioned_hashes,
            )),
        )
    }

    /// Creates a new instance from args to engine API method `newPayloadV4`.
    ///
    /// Spec: <https://specs.optimism.io/protocol/exec-engine.html#engine_newpayloadv4>
    pub fn v4(
        payload: OpExecutionPayloadV4,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Requests,
    ) -> Self {
        Self::new(
            OpExecutionPayload::v4(payload),
            OpExecutionPayloadSidecar::v4(
                CancunPayloadFields::new(parent_beacon_block_root, versioned_hashes),
                PraguePayloadFields::new(execution_requests),
            ),
        )
    }

    /// Returns the parent beacon block root, if any.
    pub fn parent_beacon_block_root(&self) -> Option<B256> {
        self.sidecar.parent_beacon_block_root()
    }

    /// Return the withdrawals for the payload or attributes.
    pub const fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        match &self.payload {
            OpExecutionPayload::V1(_) => None,
            OpExecutionPayload::V2(execution_payload_v2) => Some(&execution_payload_v2.withdrawals),
            OpExecutionPayload::V3(execution_payload_v3) => {
                Some(execution_payload_v3.withdrawals())
            }
            OpExecutionPayload::V4(op_execution_payload_v4) => {
                Some(op_execution_payload_v4.payload_inner.withdrawals())
            }
        }
    }

    /// Returns the parent hash of the block.
    pub const fn parent_hash(&self) -> B256 {
        self.payload.parent_hash()
    }

    /// Returns the hash of the block.
    pub const fn block_hash(&self) -> B256 {
        self.payload.block_hash()
    }

    /// Returns the number of the block.
    pub const fn block_number(&self) -> u64 {
        self.payload.block_number()
    }
}

/// Optimism execution payload envelope in network format.
///
/// This struct is used to represent payloads that are sent over the Optimism
/// CL p2p network in a snappy-compressed format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpNetworkPayloadEnvelope {
    /// The execution payload.
    pub payload: OpExecutionPayload,
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

        let payload = OpExecutionPayload::V1(
            alloy_rpc_types_engine::ExecutionPayloadV1::from_ssz_bytes(block_data)?,
        );

        Ok(Self { payload, signature, payload_hash: hash, parent_beacon_block_root: None })
    }

    /// Encodes a payload envelope as a snappy-compressed byte array.
    #[cfg(feature = "std")]
    pub fn encode_v1(&self) -> Result<Vec<u8>, PayloadEnvelopeEncodeError> {
        use ssz::Encode;
        let execution_payload_v1 = match &self.payload {
            OpExecutionPayload::V1(execution_payload_v1) => execution_payload_v1,
            _ => return Err(PayloadEnvelopeEncodeError::WrongVersion),
        };

        let mut data = Vec::new();
        let mut sig = self.signature.as_bytes();
        sig[64] = self.signature.v() as u8;
        data.extend_from_slice(&sig[..]);
        let block_data = execution_payload_v1.as_ssz_bytes();
        data.extend_from_slice(block_data.as_slice());

        Ok(snap::raw::Encoder::new().compress_vec(&data)?)
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

        let payload = OpExecutionPayload::V2(
            alloy_rpc_types_engine::ExecutionPayloadV2::from_ssz_bytes(block_data)?,
        );

        Ok(Self { payload, signature, payload_hash: hash, parent_beacon_block_root: None })
    }

    /// Encodes a payload envelope as a snappy-compressed byte array.
    #[cfg(feature = "std")]
    pub fn encode_v2(&self) -> Result<Vec<u8>, PayloadEnvelopeEncodeError> {
        use ssz::Encode;
        let execution_payload_v2 = match &self.payload {
            OpExecutionPayload::V2(execution_payload_v2) => execution_payload_v2,
            _ => return Err(PayloadEnvelopeEncodeError::WrongVersion),
        };

        let mut data = Vec::new();
        let mut sig = self.signature.as_bytes();
        sig[64] = self.signature.v() as u8;
        data.extend_from_slice(&sig[..]);
        let block_data = execution_payload_v2.as_ssz_bytes();
        data.extend_from_slice(block_data.as_slice());

        Ok(snap::raw::Encoder::new().compress_vec(&data)?)
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
        let parent_beacon_block_root = B256::from_slice(parent_beacon_block_root);
        let hash = PayloadHash::from(
            [parent_beacon_block_root.as_slice(), block_data].concat().as_slice(),
        );

        let payload = OpExecutionPayload::V3(
            alloy_rpc_types_engine::ExecutionPayloadV3::from_ssz_bytes(block_data)?,
        );

        Ok(Self {
            payload,
            signature,
            payload_hash: hash,
            parent_beacon_block_root: Some(parent_beacon_block_root),
        })
    }

    /// Encodes a payload envelope as a snappy-compressed byte array.
    #[cfg(feature = "std")]
    pub fn encode_v3(&self) -> Result<Vec<u8>, PayloadEnvelopeEncodeError> {
        use ssz::Encode;
        let execution_payload_v3 = match &self.payload {
            OpExecutionPayload::V3(execution_payload_v3) => execution_payload_v3,
            _ => return Err(PayloadEnvelopeEncodeError::WrongVersion),
        };

        let mut data = Vec::new();
        let mut sig = self.signature.as_bytes();
        sig[64] = self.signature.v() as u8;
        data.extend_from_slice(&sig[..]);
        data.extend_from_slice(self.parent_beacon_block_root.as_ref().unwrap().as_slice());
        let block_data = execution_payload_v3.as_ssz_bytes();
        data.extend_from_slice(block_data.as_slice());

        Ok(snap::raw::Encoder::new().compress_vec(&data)?)
    }

    /// Decode a payload envelope from a snappy-compressed byte array.
    /// The payload version decoded is `ExecutionPayloadV4` from SSZ bytes.
    #[cfg(feature = "std")]
    pub fn decode_v4(data: &[u8]) -> Result<Self, PayloadEnvelopeError> {
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
        let parent_beacon_block_root = B256::from_slice(parent_beacon_block_root);
        let hash = PayloadHash::from(
            [parent_beacon_block_root.as_slice(), block_data].concat().as_slice(),
        );

        let payload = OpExecutionPayload::V4(OpExecutionPayloadV4::from_ssz_bytes(block_data)?);

        Ok(Self {
            payload,
            signature,
            payload_hash: hash,
            parent_beacon_block_root: Some(parent_beacon_block_root),
        })
    }

    /// Encodes a payload envelope as a snappy-compressed byte array.
    #[cfg(feature = "std")]
    pub fn encode_v4(&self) -> Result<Vec<u8>, PayloadEnvelopeEncodeError> {
        use ssz::Encode;
        let execution_payload_v4 = match &self.payload {
            OpExecutionPayload::V4(execution_payload_v4) => execution_payload_v4,
            _ => return Err(PayloadEnvelopeEncodeError::WrongVersion),
        };

        let mut data = Vec::new();
        let mut sig = self.signature.as_bytes();
        sig[64] = self.signature.v() as u8;
        data.extend_from_slice(&sig[..]);
        data.extend_from_slice(self.parent_beacon_block_root.as_ref().unwrap().as_slice());
        let block_data = execution_payload_v4.as_ssz_bytes();
        data.extend_from_slice(block_data.as_slice());

        Ok(snap::raw::Encoder::new().compress_vec(&data)?)
    }
}

/// Errors that can occur when encoding a payload envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadEnvelopeEncodeError {
    /// Wrong versions of the payload.
    #[error("Wrong version of the payload")]
    WrongVersion,
    /// An error occured during snap encoding.
    #[error(transparent)]
    #[cfg(feature = "std")]
    SnapEncoding(#[from] snap::Error),
}

/// Errors that can occur when decoding a payload envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PayloadEnvelopeError {
    /// The snappy encoding is broken.
    #[error("Broken snappy encoding")]
    BrokenSnappyEncoding,
    /// The signature is invalid.
    #[error("Invalid signature")]
    InvalidSignature,
    /// The SSZ encoding is broken.
    #[error("Broken SSZ encoding")]
    BrokenSszEncoding,
    /// The payload envelope is of invalid length.
    #[error("Invalid length")]
    InvalidLength,
}

impl From<alloy_primitives::SignatureError> for PayloadEnvelopeError {
    fn from(_: alloy_primitives::SignatureError) -> Self {
        Self::InvalidSignature
    }
}

#[cfg(feature = "std")]
impl From<snap::Error> for PayloadEnvelopeError {
    fn from(_: snap::Error) -> Self {
        Self::BrokenSnappyEncoding
    }
}

#[cfg(feature = "std")]
impl From<ssz::DecodeError> for PayloadEnvelopeError {
    fn from(_: ssz::DecodeError) -> Self {
        Self::BrokenSszEncoding
    }
}

/// Represents the Keccak256 hash of the block
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
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
    #[cfg(feature = "serde")]
    fn test_serde_rountrip_op_execution_payload_envelope() {
        let envelope_str = r#"{
            "payload": {"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0","withdrawalsRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119"},
            "parentBeaconBlockRoot": "0x9999999999999999999999999999999999999999999999999999999999999999"
        }"#;

        let envelope: OpExecutionPayloadEnvelope = serde_json::from_str(envelope_str).unwrap();
        let expected = b256!("9999999999999999999999999999999999999999999999999999999999999999");
        assert_eq!(envelope.parent_beacon_block_root.unwrap(), expected);
        let _ = serde_json::to_string(&envelope).unwrap();
    }

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
    fn test_roundtrip_encode_envelope_v1() {
        use alloy_primitives::hex;
        let data = hex::decode("0xbd04f043128457c6ccf35128497167442bcc0f8cce78cda8b366e6a12e526d938d1e4c1046acffffbfc542a7e212bb7d80d3a4b2f84f7b196d935398a24eb84c519789b401000000fe0300fe0300fe0300fe0300fe0300fe0300a203000c4a8fd56621ad04fc0101067601008ce60be0005b220117c32c0f3b394b346c2aa42cfa8157cd41f891aa0bec485a62fc010000").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v1(&data).unwrap();
        assert_eq!(1725271882, payload_envelop.payload.timestamp());
        let encoded = payload_envelop.encode_v1().unwrap();
        assert_eq!(data, encoded);
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_roundtrip_encode_envelope_v2() {
        use alloy_primitives::hex;
        let data = hex::decode("0xc104f0433805080eb36c0b130a7cc1dc74c3f721af4e249aa6f61bb89d1557143e971bb738a3f3b98df7c457e74048e9d2d7e5cd82bb45e3760467e2270e9db86d1271a700000000fe0300fe0300fe0300fe0300fe0300fe0300a203000c6b89d46525ad000205067201009cda69cb5b9b73fc4eb2458b37d37f04ff507fe6c9cd2ab704a05ea9dae3cd61760002000000020000").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v2(&data).unwrap();
        assert_eq!(1708427627, payload_envelop.payload.timestamp());
        let encoded = payload_envelop.encode_v2().unwrap();
        assert_eq!(data, encoded);
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_roundtrip_encode_envelope_v3() {
        use alloy_primitives::hex;
        let data = hex::decode("0xf104f0434442b9eb38b259f5b23826e6b623e829d2fb878dac70187a1aecf42a3f9bedfd29793d1fcb5822324be0d3e12340a95855553a65d64b83e5579dffb31470df5d010000006a03000412346a1d00fe0100fe0100fe0100fe0100fe0100fe01004201000cc588d465219504100201067601007cfece77b89685f60e3663b6e0faf2de0734674eb91339700c4858c773a8ff921e014401043e0100").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v3(&data).unwrap();
        assert_eq!(1708427461, payload_envelop.payload.timestamp());
        let encoded = payload_envelop.encode_v3().unwrap();
        assert_eq!(data, encoded);
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_roundtrip_encode_envelope_v4() {
        use alloy_primitives::hex;
        let data = hex::decode("0x9105f043cee25401b6853202950d1d8a082f31a80c4fef5782c049a731f5d104b1b9b9aa7618605b420438ae98b44c8aaaebd482854473c2ae57c079286bb634bece5210000000006a03000412346a1d00fe0100fe0100fe0100fe0100fe0100fe01004201000c5766d26721950430020106f6010001440104b60100049876").unwrap();
        let payload_envelop = OpNetworkPayloadEnvelope::decode_v4(&data).unwrap();
        assert_eq!(1741842007, payload_envelop.payload.timestamp());
        let encoded = payload_envelop.encode_v4().unwrap();
        assert_eq!(data, encoded);
    }
}
