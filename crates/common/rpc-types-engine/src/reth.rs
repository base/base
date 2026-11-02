//! Reth compatibility implementations for payload types.

use alloc::vec::Vec;

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::PayloadId;
use reth_payload_primitives::{ExecutionPayload, PayloadAttributes};

use crate::{BasePayloadAttributes, ExecutionData};

impl PayloadAttributes for BasePayloadAttributes {
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        self.payload_attributes.payload_id(parent_hash)
    }

    fn timestamp(&self) -> u64 {
        self.payload_attributes.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.payload_attributes.withdrawals.as_ref()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.payload_attributes.parent_beacon_block_root
    }

    fn slot_number(&self) -> Option<u64> {
        self.payload_attributes.slot_number
    }
}

impl ExecutionPayload for ExecutionData {
    fn parent_hash(&self) -> B256 {
        self.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.block_hash()
    }

    fn block_number(&self) -> u64 {
        self.block_number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.payload.as_v2().map(|p| &p.withdrawals)
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        // A V5 payload carries the list inline; otherwise it is carried alongside the payload.
        self.block_access_list
            .as_ref()
            .or_else(|| self.payload.as_v5().map(|payload| &payload.block_access_list))
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.sidecar.parent_beacon_block_root()
    }

    fn timestamp(&self) -> u64 {
        self.payload.as_v1().timestamp
    }

    fn gas_used(&self) -> u64 {
        self.payload.as_v1().gas_used
    }

    fn gas_limit(&self) -> u64 {
        self.payload.gas_limit()
    }

    fn slot_number(&self) -> Option<u64> {
        // Only the Amsterdam V5 payload carries a slot number (EIP-7843).
        self.payload.as_v5().map(|payload| payload.slot_number)
    }

    fn transaction_count(&self) -> usize {
        self.payload.as_v1().transactions.len()
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::{Address, Bloom, U256};
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};

    use super::*;
    use crate::{BaseExecutionPayloadV4, BaseExecutionPayloadV5};

    /// Builds an Amsterdam payload carrying `block_access_list` and `slot_number`.
    fn v5_payload(block_access_list: Bytes, slot_number: u64) -> BaseExecutionPayloadV5 {
        BaseExecutionPayloadV5::new(
            BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                ExecutionPayloadV3 {
                    payload_inner: ExecutionPayloadV2 {
                        payload_inner: ExecutionPayloadV1 {
                            parent_hash: B256::ZERO,
                            fee_recipient: Address::ZERO,
                            state_root: B256::ZERO,
                            receipts_root: B256::ZERO,
                            logs_bloom: Bloom::ZERO,
                            prev_randao: B256::ZERO,
                            block_number: 7,
                            gas_limit: 30_000_000,
                            gas_used: 0,
                            timestamp: 1_800_000_000,
                            extra_data: Bytes::new(),
                            base_fee_per_gas: U256::ZERO,
                            block_hash: B256::ZERO,
                            transactions: Vec::new(),
                        },
                        withdrawals: Vec::new(),
                    },
                    blob_gas_used: 0,
                    excess_blob_gas: 0,
                },
                B256::repeat_byte(0xaa),
            ),
            block_access_list,
            slot_number,
        )
    }

    /// A V5 payload must expose both its block access list and its slot number through the reth
    /// `ExecutionPayload` trait, which consensus validation reads.
    #[test]
    fn v5_payload_exposes_block_access_list_and_slot_number() {
        let block_access_list = Bytes::from_static(b"bal");
        let data = ExecutionData::v5(
            v5_payload(block_access_list.clone(), 42),
            Vec::new(),
            B256::ZERO,
            Requests::default(),
        );

        assert_eq!(data.block_access_list(), Some(&block_access_list));
        assert_eq!(data.slot_number(), Some(42));
    }

    /// A pre-Amsterdam payload must report neither a block access list nor a slot number.
    #[test]
    fn v4_payload_has_no_block_access_list_or_slot_number() {
        let data = ExecutionData::v4(
            v5_payload(Bytes::new(), 0).payload_inner,
            Vec::new(),
            B256::ZERO,
            Requests::default(),
        );

        assert_eq!(data.block_access_list(), None);
        assert_eq!(data.slot_number(), None);
    }
}
