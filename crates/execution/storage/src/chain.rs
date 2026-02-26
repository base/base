use alloy_consensus::Header;
use base_execution_primitives::OpTransactionSigned;
use reth_storage_api::EmptyBodyStorage;

/// Optimism storage implementation.
pub type OpStorage<T = OpTransactionSigned, H = Header> = EmptyBodyStorage<T, H>;
