//! EIP-4337 EntryPoint event parsing
//!
//! Parses UserOperationEvent logs emitted by EntryPoint contracts (v0.6, v0.7, v0.8)
//!
//! This module provides a minimal `IndexedUserOperationRef` for storage efficiency.
//! The full event data can be re-hydrated from the transaction receipt when needed.

use alloy_primitives::{address, Address, B256, Log};
use alloy_sol_types::{sol, SolEvent};

/// EntryPoint v0.6 address
pub const ENTRYPOINT_V06: Address = address!("5ff137d4b0fdcd49dca30c7cf57e578a026d2789");

/// EntryPoint v0.7 address
pub const ENTRYPOINT_V07: Address = address!("0000000071727de22e5e9d8baf0edac6f37da032");

/// EntryPoint v0.8 address
pub const ENTRYPOINT_V08: Address = address!("4337084d9e255ff0702461cf8895ce9e3b5ff108");

// Define the UserOperationEvent using alloy's sol! macro
// This event is emitted by the EntryPoint contract when a UserOperation is executed
sol! {
    /// UserOperationEvent emitted by EntryPoint contracts
    /// 
    /// Event signature: UserOperationEvent(bytes32 indexed userOpHash, address indexed sender, 
    ///                                      address indexed paymaster, uint256 nonce, bool success, 
    ///                                      uint256 actualGasCost, uint256 actualGasUsed)
    #[derive(Debug)]
    event UserOperationEvent(
        bytes32 indexed userOpHash,
        address indexed sender,
        address indexed paymaster,
        uint256 nonce,
        bool success,
        uint256 actualGasCost,
        uint256 actualGasUsed
    );
}

/// Minimal reference to an indexed UserOperation
///
/// This struct contains only the data needed to locate and re-hydrate
/// the full UserOperation data from the transaction receipt.
/// This reduces storage by ~75% compared to storing all event fields.
///
/// To get full event data, use the transaction_hash to fetch the receipt,
/// then parse the log at log_index.
#[derive(Debug, Clone, Copy)]
pub struct IndexedUserOperationRef {
    /// The hash of the UserOperation (lookup key)
    pub user_op_hash: B256,
    /// Transaction hash that included this UserOp
    pub transaction_hash: B256,
    /// Log index within the transaction receipt
    pub log_index: u64,
    /// Block number (needed for reorg handling)
    pub block_number: u64,
}

/// Returns true if the address is a known EntryPoint contract
pub fn is_entry_point(address: Address) -> bool {
    address == ENTRYPOINT_V06 || address == ENTRYPOINT_V07 || address == ENTRYPOINT_V08
}

/// Try to parse a UserOperationEvent from a log and return a minimal reference
/// 
/// Returns None if:
/// - The log is not from an EntryPoint contract
/// - The log doesn't match the UserOperationEvent signature
///
/// The returned `IndexedUserOperationRef` contains only the minimal data needed
/// to locate and re-hydrate the full event data from the transaction receipt.
pub fn try_parse_user_operation_ref(
    log: &Log,
    block_number: u64,
    transaction_hash: B256,
    log_index: u64,
) -> Option<IndexedUserOperationRef> {
    // Check if this log is from an EntryPoint contract
    if !is_entry_point(log.address) {
        return None;
    }

    // Try to decode the UserOperationEvent to get the hash
    let event = UserOperationEvent::decode_log(log).ok()?;

    Some(IndexedUserOperationRef {
        user_op_hash: event.userOpHash,
        transaction_hash,
        log_index,
        block_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_entry_point() {
        assert!(is_entry_point(ENTRYPOINT_V06));
        assert!(is_entry_point(ENTRYPOINT_V07));
        assert!(is_entry_point(ENTRYPOINT_V08));
        assert!(!is_entry_point(Address::ZERO));
    }

    #[test]
    fn test_event_signature() {
        // Verify the event signature matches EIP-4337 spec
        let expected_sig = "UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)";
        assert_eq!(UserOperationEvent::SIGNATURE, expected_sig);
    }
}

