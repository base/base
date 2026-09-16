//! Bounded SSZ decoding for historical execution payload versions.

use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};

use crate::{BoundedTransactions, BoundedWithdrawals};

/// SSZ decoder for [`ExecutionPayloadV1`] with Base's payload list bounds.
///
/// Preserves the Alloy payload representation while rejecting oversized lists
/// before allocating their elements. Use this instead of Alloy's unbounded SSZ
/// decoder for untrusted network payloads.
#[derive(Debug)]
pub struct BoundedExecutionPayloadV1(pub ExecutionPayloadV1);

impl ssz::Decode for BoundedExecutionPayloadV1 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        let mut builder = ssz::SszDecoderBuilder::new(bytes);

        builder.register_type::<B256>()?;
        builder.register_type::<Address>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<Bloom>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<Bytes>()?;
        builder.register_type::<U256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<BoundedTransactions>()?;

        let mut decoder = builder.build()?;
        Ok(Self(ExecutionPayloadV1 {
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
        }))
    }
}

/// SSZ decoder for [`ExecutionPayloadV2`] with Base's payload list bounds.
///
/// Preserves the Alloy payload representation while rejecting oversized lists
/// before allocating their elements. Use this instead of Alloy's unbounded SSZ
/// decoder for untrusted network payloads.
#[derive(Debug)]
pub struct BoundedExecutionPayloadV2(pub ExecutionPayloadV2);

impl ssz::Decode for BoundedExecutionPayloadV2 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        let mut builder = ssz::SszDecoderBuilder::new(bytes);

        builder.register_type::<B256>()?;
        builder.register_type::<Address>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<Bloom>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<Bytes>()?;
        builder.register_type::<U256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<BoundedTransactions>()?;
        builder.register_type::<BoundedWithdrawals>()?;

        let mut decoder = builder.build()?;
        Ok(Self(ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
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
        }))
    }
}

/// SSZ decoder for [`ExecutionPayloadV3`] with Base's payload list bounds.
///
/// Preserves the Alloy payload representation while rejecting oversized lists
/// before allocating their elements. Use this instead of Alloy's unbounded SSZ
/// decoder for untrusted network payloads.
#[derive(Debug)]
pub struct BoundedExecutionPayloadV3(pub ExecutionPayloadV3);

impl ssz::Decode for BoundedExecutionPayloadV3 {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        let mut builder = ssz::SszDecoderBuilder::new(bytes);

        builder.register_type::<B256>()?;
        builder.register_type::<Address>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<Bloom>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<Bytes>()?;
        builder.register_type::<U256>()?;
        builder.register_type::<B256>()?;
        builder.register_type::<BoundedTransactions>()?;
        builder.register_type::<BoundedWithdrawals>()?;
        builder.register_type::<u64>()?;
        builder.register_type::<u64>()?;

        let mut decoder = builder.build()?;
        Ok(Self(ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
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
        }))
    }
}

#[cfg(test)]
mod tests {
    use ssz::{Decode, Encode};

    use super::*;

    #[test]
    fn bounded_decoders_preserve_payload_fields() {
        let v1 = ExecutionPayloadV1 {
            parent_hash: B256::repeat_byte(1),
            fee_recipient: Address::repeat_byte(2),
            state_root: B256::repeat_byte(3),
            receipts_root: B256::repeat_byte(4),
            logs_bloom: Bloom::from([5; 256]),
            prev_randao: B256::repeat_byte(6),
            block_number: 7,
            gas_limit: 8,
            gas_used: 9,
            timestamp: 10,
            extra_data: Bytes::from_static(b"extra data"),
            base_fee_per_gas: U256::from(12),
            block_hash: B256::repeat_byte(13),
            transactions: vec![
                Bytes::from_static(b"first"),
                Bytes::new(),
                Bytes::from_static(b"second"),
            ],
        };
        assert_eq!(BoundedExecutionPayloadV1::from_ssz_bytes(&v1.as_ssz_bytes()).unwrap().0, v1);

        let v2 = ExecutionPayloadV2 { payload_inner: v1, withdrawals: vec![] };
        assert_eq!(BoundedExecutionPayloadV2::from_ssz_bytes(&v2.as_ssz_bytes()).unwrap().0, v2);

        let v3 = ExecutionPayloadV3 { payload_inner: v2, blob_gas_used: 14, excess_blob_gas: 15 };
        assert_eq!(BoundedExecutionPayloadV3::from_ssz_bytes(&v3.as_ssz_bytes()).unwrap().0, v3);
    }
}
