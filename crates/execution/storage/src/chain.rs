use alloy_consensus::Header;
use base_alloy_consensus::OpTransactionSigned;
use reth_storage_api::EmptyBodyStorage;

/// Base storage implementation.
pub type OpStorage<T = OpTransactionSigned, H = Header> = EmptyBodyStorage<T, H>;
