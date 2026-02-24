//! `AccountResult` type matching the Go `eth.AccountResult` struct.
//!
//! This matches the standard Ethereum JSON-RPC `eth_getProof` response format.
//! Uses camelCase field names (different from Proposal's `PascalCase`).

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Encodable;
use alloy_trie::{Nibbles, proof::verify_proof};
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;

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
    /// Creates a new `AccountResult`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::missing_const_for_fn)] // Not const: Vec params can't be constructed in const context
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

    /// Verify the account proof against a state root.
    ///
    /// This verifies that:
    /// 1. The account proof is valid against the state root
    /// 2. The account data (nonce, balance, `storage_hash`, `code_hash`) matches the proof
    ///
    /// # Arguments
    ///
    /// * `state_root` - The state root to verify against
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::AccountProofFailed` if:
    /// - The proof is invalid
    /// - The account data doesn't match the proof
    pub fn verify(&self, state_root: B256) -> Result<(), ProviderError> {
        // Convert account proof to the format expected by verify_proof
        let proof: Vec<Bytes> = self.account_proof.clone();

        // The key in the state trie is keccak256(address), converted to nibbles
        let key_hash = keccak256(self.address);
        let key = Nibbles::unpack(key_hash);

        // RLP encode the account: (nonce, balance, storage_hash, code_hash)
        // Account nonces in Ethereum are u64, so this conversion is safe.
        // U256 representation is for JSON-RPC compatibility.
        let nonce: u64 = self
            .nonce
            .try_into()
            .map_err(|_| ProviderError::AccountProofFailed("nonce exceeds u64::MAX".to_string()))?;
        let account = Account {
            nonce,
            balance: self.balance,
            storage_root: self.storage_hash,
            code_hash: self.code_hash,
        };
        let mut expected_value = Vec::new();
        account.encode(&mut expected_value);

        // Verify the proof
        verify_proof(state_root, key, Some(expected_value), &proof)
            .map_err(|e| ProviderError::AccountProofFailed(e.to_string()))
    }
}

/// Account structure for RLP encoding.
///
/// This matches the Ethereum account structure in the state trie.
#[derive(Debug, Clone)]
struct Account {
    nonce: u64,
    balance: U256,
    storage_root: B256,
    code_hash: B256,
}

impl Encodable for Account {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        // RLP encode as a list: [nonce, balance, storage_root, code_hash]
        let header = alloy_rlp::Header {
            list: true,
            payload_length: self.nonce.length()
                + self.balance.length()
                + self.storage_root.length()
                + self.code_hash.length(),
        };
        header.encode(out);
        self.nonce.encode(out);
        self.balance.encode(out);
        self.storage_root.encode(out);
        self.code_hash.encode(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.nonce.length()
            + self.balance.length()
            + self.storage_root.length()
            + self.code_hash.length();
        alloy_rlp::length_of_length(payload_length) + payload_length
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
