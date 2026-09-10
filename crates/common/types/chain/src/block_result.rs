//! Receipts, requests, and gas totals produced by block execution.

use alloc::vec::Vec;

use alloy_eips::eip7685::Requests;

/// The result of executing a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockExecutionResult<T> {
    /// All the receipts of the transactions in the block.
    pub receipts: Vec<T>,
    /// All the EIP-7685 requests in the block.
    pub requests: Requests,
    /// The total gas used by the block.
    pub gas_used: u64,
    /// Blob gas used by the block.
    pub blob_gas_used: u64,
}

impl<T> Default for BlockExecutionResult<T> {
    fn default() -> Self {
        Self {
            receipts: Default::default(),
            requests: Default::default(),
            gas_used: 0,
            blob_gas_used: 0,
        }
    }
}
