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
        None
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
