//! Reth compatibility implementations for payload types.

use alloc::vec::Vec;

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::PayloadId;
use reth_payload_primitives::{ExecutionPayload, PayloadAttributes};

use crate::{BasePayloadAttributes, ExecutionData, TracedExecutionData};

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
        self.block_access_list.as_ref()
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
        None
    }

    fn transaction_count(&self) -> usize {
        self.payload.as_v1().transactions.len()
    }
}

impl ExecutionPayload for TracedExecutionData {
    fn parent_hash(&self) -> B256 {
        ExecutionPayload::parent_hash(&self.inner)
    }

    fn block_hash(&self) -> B256 {
        ExecutionPayload::block_hash(&self.inner)
    }

    fn block_number(&self) -> u64 {
        ExecutionPayload::block_number(&self.inner)
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        ExecutionPayload::withdrawals(&self.inner)
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        ExecutionPayload::block_access_list(&self.inner)
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        ExecutionPayload::parent_beacon_block_root(&self.inner)
    }

    fn timestamp(&self) -> u64 {
        ExecutionPayload::timestamp(&self.inner)
    }

    fn gas_used(&self) -> u64 {
        ExecutionPayload::gas_used(&self.inner)
    }

    fn gas_limit(&self) -> u64 {
        ExecutionPayload::gas_limit(&self.inner)
    }

    fn slot_number(&self) -> Option<u64> {
        ExecutionPayload::slot_number(&self.inner)
    }

    fn transaction_count(&self) -> usize {
        ExecutionPayload::transaction_count(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes};
    use base_common_consensus::BaseBlock;
    use reth_payload_primitives::ExecutionPayload as _;

    use super::{ExecutionData, TracedExecutionData};

    #[test]
    fn traced_execution_data_preserves_block_access_list() {
        let expected = Bytes::from_static(b"block-access-list");
        let execution_data = ExecutionData::from_block_unchecked_with_extras(
            B256::ZERO,
            &BaseBlock::default(),
            Some(expected.clone()),
        );

        let traced = TracedExecutionData::from(execution_data);

        assert_eq!(traced.block_access_list(), Some(&expected));
    }
}
