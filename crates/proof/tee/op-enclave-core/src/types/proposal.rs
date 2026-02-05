//! Proposal type matching the Go `Proposal` struct.
//!
//! This matches the Go struct from `server.go`:
//! ```go
//! type Proposal struct {
//!     OutputRoot     common.Hash
//!     Signature      hexutil.Bytes
//!     L1OriginHash   common.Hash
//!     L2BlockNumber  *hexutil.Big
//!     PrevOutputRoot common.Hash
//!     ConfigHash     common.Hash
//! }
//! ```

use alloy_primitives::{B256, Bytes, U256};
use serde::{Deserialize, Serialize};

use crate::serde_utils::{bytes_hex, u256_hex};

/// A proposal containing an output root and signature.
///
/// Field names use PascalCase to match Go's default JSON encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Proposal {
    /// The output root hash.
    pub output_root: B256,

    /// The ECDSA signature (65 bytes: r, s, v).
    #[serde(with = "bytes_hex", rename = "Signature")]
    pub signature: Bytes,

    /// The L1 origin block hash.
    #[serde(rename = "L1OriginHash")]
    pub l1_origin_hash: B256,

    /// The L2 block number.
    #[serde(with = "u256_hex", rename = "L2BlockNumber")]
    pub l2_block_number: U256,

    /// The previous output root hash.
    pub prev_output_root: B256,

    /// The config hash.
    pub config_hash: B256,
}

impl Proposal {
    /// Creates a new Proposal.
    pub fn new(
        output_root: B256,
        signature: Bytes,
        l1_origin_hash: B256,
        l2_block_number: U256,
        prev_output_root: B256,
        config_hash: B256,
    ) -> Self {
        Self {
            output_root,
            signature,
            l1_origin_hash,
            l2_block_number,
            prev_output_root,
            config_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    fn sample_proposal() -> Proposal {
        Proposal {
            output_root: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: b256!(
                "0000000000000000000000000000000000000000000000000000000000000002"
            ),
            l2_block_number: U256::from(12345),
            prev_output_root: b256!(
                "0000000000000000000000000000000000000000000000000000000000000003"
            ),
            config_hash: b256!("0000000000000000000000000000000000000000000000000000000000000004"),
        }
    }

    #[test]
    fn test_proposal_serialization_pascal_case() {
        let proposal = sample_proposal();
        let json = serde_json::to_string(&proposal).unwrap();

        // Verify PascalCase field names
        assert!(json.contains("\"OutputRoot\""));
        assert!(json.contains("\"Signature\""));
        assert!(json.contains("\"L1OriginHash\""));
        assert!(json.contains("\"L2BlockNumber\""));
        assert!(json.contains("\"PrevOutputRoot\""));
        assert!(json.contains("\"ConfigHash\""));

        // Verify no snake_case fields
        assert!(!json.contains("\"output_root\""));
        assert!(!json.contains("\"l1_origin_hash\""));
    }

    #[test]
    fn test_proposal_hex_encoding() {
        let proposal = sample_proposal();
        let json = serde_json::to_string(&proposal).unwrap();

        // Verify 0x prefix
        assert!(json.contains("\"0x"));

        // Verify L2BlockNumber is minimal hex (12345 = 0x3039)
        assert!(json.contains("\"0x3039\""));
    }

    #[test]
    fn test_proposal_roundtrip() {
        let original = sample_proposal();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_proposal_deserialize_go_format() {
        // JSON that matches Go output format
        let go_json = r#"{
            "OutputRoot": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "Signature": "0xababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab",
            "L1OriginHash": "0x0000000000000000000000000000000000000000000000000000000000000002",
            "L2BlockNumber": "0x3039",
            "PrevOutputRoot": "0x0000000000000000000000000000000000000000000000000000000000000003",
            "ConfigHash": "0x0000000000000000000000000000000000000000000000000000000000000004"
        }"#;

        let proposal: Proposal = serde_json::from_str(go_json).unwrap();

        assert_eq!(
            proposal.output_root,
            b256!("0000000000000000000000000000000000000000000000000000000000000001")
        );
        assert_eq!(proposal.signature.len(), 65);
        assert_eq!(proposal.l2_block_number, U256::from(12345));
    }

    #[test]
    fn test_proposal_l2_block_number_zero() {
        let mut proposal = sample_proposal();
        proposal.l2_block_number = U256::ZERO;

        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("\"L2BlockNumber\":\"0x0\""));
    }

    #[test]
    fn test_proposal_pretty_json_matches_go() {
        let proposal = sample_proposal();
        let json = serde_json::to_value(&proposal).unwrap();

        // Extract field names from JSON object
        let obj = json.as_object().unwrap();
        let keys: Vec<&String> = obj.keys().collect();

        assert!(keys.contains(&&"OutputRoot".to_string()));
        assert!(keys.contains(&&"Signature".to_string()));
        assert!(keys.contains(&&"L1OriginHash".to_string()));
        assert!(keys.contains(&&"L2BlockNumber".to_string()));
        assert!(keys.contains(&&"PrevOutputRoot".to_string()));
        assert!(keys.contains(&&"ConfigHash".to_string()));
    }
}
