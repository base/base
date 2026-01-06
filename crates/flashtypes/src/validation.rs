use alloy_primitives::{Address, B256};

/// Returns true if the address is non-zero.
pub fn is_non_zero_address(address: Address) -> bool {
    address != Address::ZERO
}

/// Returns true if the state root is non-zero.
pub fn is_valid_state_root(state_root: B256) -> bool {
    state_root != B256::ZERO
}

/// Returns true if the block hash is non-zero.
pub fn is_valid_block_hash(block_hash: B256) -> bool {
    block_hash != B256::ZERO
}

/// Returns true if blob gas is either absent or positive.
pub fn is_valid_blob_gas(blob_gas_used: Option<u64>) -> bool {
    blob_gas_used.map_or(true, |value| value > 0)
}

/// Returns true if gas used does not exceed the provided limit.
pub fn is_valid_gas_usage(gas_used: u64, gas_limit: u64) -> bool {
    gas_limit > 0 && gas_used <= gas_limit
}

/// Returns true if the timestamp is non-zero.
pub fn is_valid_timestamp(timestamp: u64) -> bool {
    timestamp > 0
}

/// Returns true if the transaction bytes vector is non-empty.
pub fn is_valid_transaction_bytes(bytes: &[u8]) -> bool {
    !bytes.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_addresses() {
        assert!(is_non_zero_address(Address::from([1u8; 20])));
        assert!(!is_non_zero_address(Address::ZERO));
    }

    #[test]
    fn validates_hashes() {
        assert!(is_valid_state_root(B256::from([0xAAu8; 32])));
        assert!(!is_valid_state_root(B256::ZERO));
        assert!(is_valid_block_hash(B256::from([0x11u8; 32])));
        assert!(!is_valid_block_hash(B256::ZERO));
    }

    #[test]
    fn validates_blob_gas() {
        assert!(is_valid_blob_gas(None));
        assert!(is_valid_blob_gas(Some(1)));
        assert!(!is_valid_blob_gas(Some(0)));
    }

    #[test]
    fn validates_gas_usage() {
        assert!(is_valid_gas_usage(5, 10));
        assert!(is_valid_gas_usage(0, 1));
        assert!(!is_valid_gas_usage(11, 10));
        assert!(!is_valid_gas_usage(1, 0));
    }

    #[test]
    fn validates_timestamp_and_tx_bytes() {
        assert!(is_valid_timestamp(1));
        assert!(!is_valid_timestamp(0));
        assert!(is_valid_transaction_bytes(&[0x01, 0x02]));
        assert!(!is_valid_transaction_bytes(&[]));
    }
}
