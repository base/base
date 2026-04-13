use alloy_consensus::Header;
use base_common_consensus::BaseTransactionSigned;
use reth_storage_api::EmptyBodyStorage;

/// Base storage implementation.
pub type BaseStorage<T = BaseTransactionSigned, H = Header> = EmptyBodyStorage<T, H>;
