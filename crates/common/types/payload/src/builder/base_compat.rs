//! Reth compatibility implementations for payload types.

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::Bytes;

use crate::ExecutionData;

impl ExecutionData {
    /// Returns the block number and hash.
    pub fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.block_number(), self.block_hash())
    }

    /// Returns this block and its parent hash.
    pub fn block_with_parent(&self) -> BlockWithParent {
        BlockWithParent::new(self.parent_hash(), self.num_hash())
    }

    /// Returns the encoded block access list.
    pub fn block_access_list(&self) -> Option<&Bytes> {
        self.block_access_list.as_ref()
    }

    /// Returns the payload timestamp.
    pub fn timestamp(&self) -> u64 {
        self.payload.as_v1().timestamp
    }

    /// Returns the gas consumed by the payload.
    pub fn gas_used(&self) -> u64 {
        self.payload.as_v1().gas_used
    }

    /// Returns the payload gas limit.
    pub fn gas_limit(&self) -> u64 {
        self.payload.gas_limit()
    }

    /// Base execution payloads do not carry a slot number.
    pub fn slot_number(&self) -> Option<u64> {
        None
    }

    /// Returns the number of payload transactions.
    pub fn transaction_count(&self) -> usize {
        self.payload.as_v1().transactions.len()
    }
}
