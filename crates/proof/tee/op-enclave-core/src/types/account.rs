//! AccountResult type matching the Go `eth.AccountResult` struct.
//!
//! This matches the standard Ethereum JSON-RPC `eth_getProof` response format.
//! Uses camelCase field names (different from Proposal's PascalCase).

use alloy_primitives::{Address, B256, Bytes, U256};
use serde::{Deserialize, Serialize};

/// A storage proof for a single storage slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageProof {
    /// The storage key.
    pub key: B256,

    /// The value at the storage key.
    pub value: U256,

    /// The Merkle proof for this storage slot.
    pub proof: Vec<Bytes>,
}

/// Account proof result from `eth_getProof`.
///
/// Field names use camelCase to match Ethereum JSON-RPC format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountResult {
    /// The account address.
    pub address: Address,

    /// The Merkle proof for the account.
    pub account_proof: Vec<Bytes>,

    /// The account balance.
    pub balance: U256,

    /// The code hash of the account.
    pub code_hash: B256,

    /// The account nonce.
    pub nonce: U256,

    /// The storage root hash.
    pub storage_hash: B256,

    /// Storage proofs for requested slots.
    pub storage_proof: Vec<StorageProof>,
}

impl AccountResult {
    /// Creates a new AccountResult.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        account_proof: Vec<Bytes>,
        balance: U256,
        code_hash: B256,
        nonce: U256,
        storage_hash: B256,
        storage_proof: Vec<StorageProof>,
    ) -> Self {
        Self {
            address,
            account_proof,
            balance,
            code_hash,
            nonce,
            storage_hash,
            storage_proof,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn sample_account_result() -> AccountResult {
        AccountResult {
            address: address!("4200000000000000000000000000000000000016"),
            account_proof: vec![Bytes::from(vec![0xab, 0xcd])],
            balance: U256::ZERO,
            code_hash: b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"),
            nonce: U256::ZERO,
            storage_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            storage_proof: vec![StorageProof {
                key: b256!("0000000000000000000000000000000000000000000000000000000000000000"),
                value: U256::from(42),
                proof: vec![Bytes::from(vec![0xde, 0xad])],
            }],
        }
    }

    #[test]
    fn test_account_result_camel_case() {
        let account = sample_account_result();
        let json = serde_json::to_string(&account).unwrap();

        // Verify camelCase field names
        assert!(json.contains("\"address\""));
        assert!(json.contains("\"accountProof\""));
        assert!(json.contains("\"balance\""));
        assert!(json.contains("\"codeHash\""));
        assert!(json.contains("\"nonce\""));
        assert!(json.contains("\"storageHash\""));
        assert!(json.contains("\"storageProof\""));

        // Verify no PascalCase or snake_case
        assert!(!json.contains("\"AccountProof\""));
        assert!(!json.contains("\"account_proof\""));
    }

    #[test]
    fn test_account_result_roundtrip() {
        let original = sample_account_result();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: AccountResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_account_result_deserialize_rpc_format() {
        // JSON that matches Ethereum JSON-RPC format
        let rpc_json = r#"{
            "address": "0x4200000000000000000000000000000000000016",
            "accountProof": ["0xabcd"],
            "balance": "0x0",
            "codeHash": "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            "nonce": "0x0",
            "storageHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "storageProof": [{
                "key": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "value": "0x2a",
                "proof": ["0xdead"]
            }]
        }"#;

        let account: AccountResult = serde_json::from_str(rpc_json).unwrap();

        assert_eq!(
            account.address,
            address!("4200000000000000000000000000000000000016")
        );
        assert_eq!(account.storage_proof.len(), 1);
        assert_eq!(account.storage_proof[0].value, U256::from(42));
    }

    #[test]
    fn test_storage_proof_serialization() {
        let storage_proof = StorageProof {
            key: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            value: U256::from(100),
            proof: vec![Bytes::from(vec![0x11, 0x22, 0x33])],
        };

        let json = serde_json::to_string(&storage_proof).unwrap();
        assert!(json.contains("\"key\""));
        assert!(json.contains("\"value\""));
        assert!(json.contains("\"proof\""));
    }
}
