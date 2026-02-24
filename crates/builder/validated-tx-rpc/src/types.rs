//! Types for the validated transactions RPC endpoint.

use alloy_eips::eip2930::AccessList;
use alloy_primitives::{Address, Bytes, TxHash, U256};
use serde::{Deserialize, Serialize};

/// Serde module for base64 encoding/decoding of `Bytes`.
///
/// Use with `#[serde(with = "base64_bytes")]` on `Bytes` fields.
pub mod base64_bytes {
    use alloy_primitives::Bytes;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    /// Serializes `Bytes` as a base64-encoded string.
    pub fn serialize<S>(bytes: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = STANDARD.encode(bytes.as_ref());
        serializer.serialize_str(&encoded)
    }

    /// Deserializes a base64-encoded string into `Bytes`.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let decoded = STANDARD.decode(&s).map_err(D::Error::custom)?;
        Ok(Bytes::from(decoded))
    }
}

/// Signature components for a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionSignature {
    /// Recovery ID.
    ///
    /// For EIP-155+ transactions, this is 0 or 1.
    /// For legacy transactions, this may be 27 or 28 (or 27 + `chain_id` * 2 + 35).
    pub v: u64,
    /// R component of the signature.
    pub r: U256,
    /// S component of the signature.
    pub s: U256,
}

/// A pre-validated transaction with base64-encoded byte fields.
///
/// This represents a transaction that has already been validated by the sender
/// (e.g., the mempool node) and includes the recovered sender address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedTransaction {
    /// Recovered sender address.
    ///
    /// This is the address that was recovered from the signature.
    /// The RPC trusts that this has been verified by the caller.
    pub from: Address,

    /// Transaction type.
    ///
    /// - `0` = Legacy
    /// - `1` = EIP-2930 (access list)
    /// - `2` = EIP-1559 (dynamic fee)
    /// - `4` = EIP-7702 (set code)
    pub tx_type: u8,

    /// Chain ID (required for EIP-155+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,

    /// Transaction nonce.
    pub nonce: u64,

    /// Recipient address.
    ///
    /// `None` for contract creation transactions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,

    /// Value in wei.
    pub value: U256,

    /// Gas limit.
    pub gas_limit: u64,

    /// Gas price (legacy and EIP-2930 transactions).
    ///
    /// Mutually exclusive with `max_fee_per_gas`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<u128>,

    /// Maximum fee per gas (EIP-1559+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_gas: Option<u128>,

    /// Maximum priority fee per gas (EIP-1559+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_priority_fee_per_gas: Option<u128>,

    /// Input data (calldata) - BASE64 ENCODED.
    ///
    /// This field uses base64 encoding instead of hex for efficiency.
    #[serde(with = "base64_bytes")]
    pub input: Bytes,

    /// Access list (EIP-2930+ transactions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_list: Option<AccessList>,

    /// Transaction signature.
    pub signature: TransactionSignature,
}

/// Result of inserting a single transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertResult {
    /// Transaction hash.
    pub tx_hash: TxHash,
    /// Whether the insertion succeeded.
    pub success: bool,
    /// Error message if the insertion failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl InsertResult {
    /// Creates a successful result.
    pub const fn success(tx_hash: TxHash) -> Self {
        Self { tx_hash, success: true, error: None }
    }

    /// Creates a failed result.
    pub fn failure(tx_hash: TxHash, error: impl Into<String>) -> Self {
        Self { tx_hash, success: false, error: Some(error.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_roundtrip() {
        let original = Bytes::from(vec![0x01, 0x02, 0x03, 0xde, 0xad, 0xbe, 0xef]);

        // Serialize to JSON
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            #[serde(with = "base64_bytes")]
            data: Bytes,
        }

        let wrapper = Wrapper { data: original.clone() };
        let json = serde_json::to_string(&wrapper).unwrap();

        // Should be base64, not hex
        assert!(json.contains("AQIDrevv")); // base64 of [1,2,3,0xde,0xad,0xbe,0xef]
        assert!(!json.contains("0x"));

        // Deserialize back
        let decoded: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.data, original);
    }
}
