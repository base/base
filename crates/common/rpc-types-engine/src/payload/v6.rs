//! Execution payload envelope V6.

use alloc::vec::Vec;

use alloy_consensus::Block;
use alloy_eips::Decodable2718;
use alloy_primitives::{Bytes, U256, keccak256};
use alloy_rpc_types_engine::{BlobsBundleV2, PayloadError};

use super::v4::BaseExecutionPayloadV4;
#[cfg(feature = "std")]
use super::v4::{BoundedTransactions, BoundedWithdrawals};

/// The Base execution payload for `newPayloadV5` of the engine API introduced with Amsterdam.
///
/// The Base variant follows the same pattern as V4: it replaces `ExecutionPayloadV3` with
/// [`BaseExecutionPayloadV4`] (which adds `withdrawalsRoot`) and adds the two Amsterdam fields
/// defined by the mainnet [`ExecutionPayloadV4`](alloy_rpc_types_engine::ExecutionPayloadV4):
///
/// - `blockAccessList`: the RLP-encoded block access list introduced by
///   [EIP-7928](https://eips.ethereum.org/EIPS/eip-7928). Its `keccak256` commitment is carried by
///   the block header as `block_access_list_hash`.
/// - `slotNumber`: the slot number introduced by
///   [EIP-7843](https://eips.ethereum.org/EIPS/eip-7843). Base has no beacon slots, so this
///   carries the L2 block number of the payload instead. This matches the value the payload
///   builder writes into the header (`Header::slot_number`), which consensus validation compares
///   against.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BaseExecutionPayloadV5 {
    /// L1 execution payload (V4 with Base's `withdrawalsRoot`).
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub payload_inner: BaseExecutionPayloadV4,
    /// RLP-encoded block access list as defined in [EIP-7928].
    ///
    /// [EIP-7928]: https://eips.ethereum.org/EIPS/eip-7928
    pub block_access_list: Bytes,
    /// The L2 block number of this payload, carried in the EIP-7843 `slotNumber` field.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub slot_number: u64,
}

impl BaseExecutionPayloadV5 {
    /// Wraps a [`BaseExecutionPayloadV4`] with the Amsterdam block access list and slot number.
    pub const fn new(
        payload_inner: BaseExecutionPayloadV4,
        block_access_list: Bytes,
        slot_number: u64,
    ) -> Self {
        Self { payload_inner, block_access_list, slot_number }
    }

    /// Converts [`BaseExecutionPayloadV5`] to [`Block`] with raw transactions.
    ///
    /// This performs the same conversion as the underlying V4 payload, then commits the block
    /// access list from the header and sets the slot number.
    pub fn into_block_raw(self) -> Result<Block<Bytes>, PayloadError> {
        let mut base_block = self.payload_inner.into_block_raw()?;

        // EIP-7928 commits to the list from the block header; EIP-7843 carries the slot number.
        base_block.header.block_access_list_hash = Some(keccak256(&self.block_access_list));
        base_block.header.slot_number = Some(self.slot_number);

        Ok(base_block)
    }

    /// Converts [`BaseExecutionPayloadV5`] to [`Block`].
    ///
    /// This performs the same conversion as the underlying V4 payload, then commits the block
    /// access list from the header and sets the slot number.
    ///
    /// See also [`BaseExecutionPayloadV4::try_into_block`].
    pub fn try_into_block<T: Decodable2718>(self) -> Result<Block<T>, PayloadError> {
        let block = self.into_block_raw()?;
        block.try_map_transactions(|tx| {
            T::decode_2718_exact(tx.as_ref())
                .map_err(alloy_rlp::Error::from)
                .map_err(PayloadError::from)
        })
    }

    /// Converts [`BaseExecutionPayloadV5`] to [`Block`] with a custom transaction mapper.
    ///
    /// This performs the same conversion as the underlying V4 payload, then commits the block
    /// access list from the header and sets the slot number.
    ///
    /// See also [`BaseExecutionPayloadV4::try_into_block_with`].
    pub fn try_into_block_with<T, F, E>(self, f: F) -> Result<Block<T>, PayloadError>
    where
        F: FnMut(Bytes) -> Result<T, E>,
        E: Into<PayloadError>,
    {
        let block = self.into_block_raw()?;
        block.try_map_transactions(f).map_err(|e| e.into())
    }
}

#[cfg(feature = "std")]
impl ssz::Decode for BaseExecutionPayloadV5 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        let mut builder = ssz::SszDecoderBuilder::new(bytes);

        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<alloy_primitives::Address>()?;
        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<alloy_primitives::Bloom>()?;
        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<Bytes>()?;
        builder.register_type::<U256>()?;
        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<BoundedTransactions>()?;
        builder.register_type::<BoundedWithdrawals>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<alloy_primitives::B256>()?;
        builder.register_type::<Bytes>()?;
        builder.register_type::<u64>()?;

        let mut decoder = builder.build()?;

        Ok(Self {
            payload_inner: BaseExecutionPayloadV4 {
                payload_inner: alloy_rpc_types_engine::ExecutionPayloadV3 {
                    payload_inner: alloy_rpc_types_engine::ExecutionPayloadV2 {
                        payload_inner: alloy_rpc_types_engine::ExecutionPayloadV1 {
                            parent_hash: decoder.decode_next()?,
                            fee_recipient: decoder.decode_next()?,
                            state_root: decoder.decode_next()?,
                            receipts_root: decoder.decode_next()?,
                            logs_bloom: decoder.decode_next()?,
                            prev_randao: decoder.decode_next()?,
                            block_number: decoder.decode_next()?,
                            gas_limit: decoder.decode_next()?,
                            gas_used: decoder.decode_next()?,
                            timestamp: decoder.decode_next()?,
                            extra_data: decoder.decode_next()?,
                            base_fee_per_gas: decoder.decode_next()?,
                            block_hash: decoder.decode_next()?,
                            transactions: decoder.decode_next::<BoundedTransactions>()?.0,
                        },
                        withdrawals: decoder.decode_next::<BoundedWithdrawals>()?.0,
                    },
                    blob_gas_used: decoder.decode_next()?,
                    excess_blob_gas: decoder.decode_next()?,
                },
                withdrawals_root: decoder.decode_next()?,
            },
            block_access_list: decoder.decode_next()?,
            slot_number: decoder.decode_next()?,
        })
    }
}

#[cfg(feature = "std")]
impl ssz::Encode for BaseExecutionPayloadV5 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let offset = <alloy_primitives::B256 as ssz::Encode>::ssz_fixed_len() * 6
            + <alloy_primitives::Address as ssz::Encode>::ssz_fixed_len()
            + <alloy_primitives::Bloom as ssz::Encode>::ssz_fixed_len()
            + <u64 as ssz::Encode>::ssz_fixed_len() * 7
            + <U256 as ssz::Encode>::ssz_fixed_len()
            + ssz::BYTES_PER_LENGTH_OFFSET * 4;

        let mut encoder = ssz::SszEncoder::container(buf, offset);

        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.parent_hash);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.fee_recipient);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.state_root);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.receipts_root);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.logs_bloom);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.prev_randao);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.block_number);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.gas_limit);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.gas_used);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.timestamp);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.extra_data);
        encoder
            .append(&self.payload_inner.payload_inner.payload_inner.payload_inner.base_fee_per_gas);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.block_hash);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.payload_inner.transactions);
        encoder.append(&self.payload_inner.payload_inner.payload_inner.withdrawals);
        encoder.append(&self.payload_inner.payload_inner.blob_gas_used);
        encoder.append(&self.payload_inner.payload_inner.excess_blob_gas);
        encoder.append(&self.payload_inner.withdrawals_root);
        encoder.append(&self.block_access_list);
        encoder.append(&self.slot_number);

        encoder.finalize();
    }

    fn ssz_bytes_len(&self) -> usize {
        <BaseExecutionPayloadV4 as ssz::Encode>::ssz_bytes_len(&self.payload_inner)
            + <Bytes as ssz::Encode>::ssz_bytes_len(&self.block_access_list)
            + <u64 as ssz::Encode>::ssz_fixed_len()
    }
}

/// This structure maps to the return value of `engine_getPayload` in the beacon chain spec, for
/// V6.
///
/// The Base variant follows the same pattern as V5: it uses [`BaseExecutionPayloadV5`] for the
/// execution payload and otherwise follows the V6 envelope shape.
///
/// See also:
/// [execution payload envelope v6] <https://specs.base.org/upgrades/denim/exec-engine#engine-api-usage>
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BaseExecutionPayloadEnvelopeV6 {
    /// Execution payload V5
    pub execution_payload: BaseExecutionPayloadV5,
    /// The expected value to be received by the feeRecipient in wei
    pub block_value: U256,
    /// The blobs, commitments, and proofs associated with the executed payload.
    pub blobs_bundle: BlobsBundleV2,
    /// Introduced in V3, this represents a suggestion from the execution layer if the payload
    /// should be used instead of an externally provided one.
    pub should_override_builder: bool,
    /// EIP-7685 execution layer requests.
    pub execution_requests: Vec<Bytes>,
}

#[cfg(test)]
#[cfg(any(feature = "serde", feature = "std"))]
mod tests {
    use super::*;

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip_execution_payload_envelope_v6() {
        let response = r#"{"executionPayload":{"parentHash":"0xe927a1448525fb5d32cb50ee1408461a945ba6c39bd5cf5621407d500ecc8de9","feeRecipient":"0x0000000000000000000000000000000000000000","stateRoot":"0x10f8a0830000e8edef6d00cc727ff833f064b1950afd591ae41357f97e543119","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prevRandao":"0xe0d8b4521a7da1582a713244ffb6a86aa1726932087386e2dc7973f43fc6cb24","blockNumber":"0x1","gasLimit":"0x2ffbd2","gasUsed":"0x0","timestamp":"0x1235","extraData":"0xd883010d00846765746888676f312e32312e30856c696e7578","baseFeePerGas":"0x342770c0","blockHash":"0x44d0fa5f2f73a938ebb96a2a21679eb8dea3e7b7dd8fd9f35aa756dda8bf0a8a","transactions":[],"withdrawals":[],"blobGasUsed":"0x0","excessBlobGas":"0x0","withdrawalsRoot":"0x123400000000000000000000000000000000000000000000000000000000babe","blockAccessList":"0xdeadbeef","slotNumber":"0x2a"},"blockValue":"0x0","blobsBundle":{"commitments":[],"proofs":[],"blobs":[]},"shouldOverrideBuilder":false,"executionRequests":[]}"#;
        let envelope: BaseExecutionPayloadEnvelopeV6 = serde_json::from_str(response).unwrap();
        assert_eq!(serde_json::to_string(&envelope).unwrap(), response);
        assert_eq!(envelope.execution_payload.slot_number, 42);
    }

    #[cfg(feature = "std")]
    fn sample_payload(block_access_list: Bytes, slot_number: u64) -> BaseExecutionPayloadV5 {
        BaseExecutionPayloadV5::new(
            BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                alloy_rpc_types_engine::ExecutionPayloadV3 {
                    payload_inner: alloy_rpc_types_engine::ExecutionPayloadV2 {
                        payload_inner: alloy_rpc_types_engine::ExecutionPayloadV1 {
                            parent_hash: alloy_primitives::B256::ZERO,
                            fee_recipient: alloy_primitives::Address::ZERO,
                            state_root: alloy_primitives::B256::ZERO,
                            receipts_root: alloy_primitives::B256::ZERO,
                            logs_bloom: alloy_primitives::Bloom::ZERO,
                            prev_randao: alloy_primitives::B256::ZERO,
                            block_number: 7,
                            gas_limit: 30_000_000,
                            gas_used: 0,
                            timestamp: 1_800_000_000,
                            extra_data: Bytes::new(),
                            base_fee_per_gas: U256::ZERO,
                            block_hash: alloy_primitives::B256::ZERO,
                            transactions: Vec::new(),
                        },
                        withdrawals: Vec::new(),
                    },
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                },
                alloy_primitives::B256::repeat_byte(0xaa),
            ),
            block_access_list,
            slot_number,
        )
    }

    #[cfg(feature = "std")]
    #[test]
    fn into_block_raw_commits_to_the_block_access_list_and_slot_number() {
        let block_access_list = Bytes::from_static(b"bal");
        let slot_number = 42;
        let payload = sample_payload(block_access_list.clone(), slot_number);

        let block = payload.into_block_raw().unwrap();
        assert_eq!(block.header.block_access_list_hash, Some(keccak256(&block_access_list)));
        assert_eq!(block.header.slot_number, Some(slot_number));
        assert_eq!(block.header.withdrawals_root, Some(alloy_primitives::B256::repeat_byte(0xaa)));
    }

    #[cfg(feature = "std")]
    #[test]
    fn ssz_roundtrip_preserves_block_access_list_and_slot_number() {
        let payload = sample_payload(Bytes::from_static(b"bal"), 42);

        let encoded = ssz::Encode::as_ssz_bytes(&payload);
        let decoded = <BaseExecutionPayloadV5 as ssz::Decode>::from_ssz_bytes(&encoded).unwrap();

        assert_eq!(decoded, payload);
    }
}
