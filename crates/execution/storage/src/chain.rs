use alloy_consensus::Header;
use base_alloy_consensus::BaseTransactionSigned;
use reth_storage_api::EmptyBodyStorage;

/// Base storage implementation.
pub type OpStorage<T = BaseTransactionSigned, H = Header> = EmptyBodyStorage<T, H>;
