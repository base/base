use alloy_primitives::{B256, Bytes, U256};

/// ECDSA signature length (r + s + v).
pub const SIGNATURE_LENGTH: usize = 65;

/// A proposal containing an output root and signature.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Proposal {
    /// The output root hash.
    pub output_root: B256,
    /// The ECDSA signature (65 bytes: r, s, v).
    pub signature: Bytes,
    /// The L1 origin block hash.
    pub l1_origin_hash: B256,
    /// The L1 origin block number.
    pub l1_origin_number: U256,
    /// The L2 block number (ending block of this proposal's range).
    pub l2_block_number: U256,
    /// The previous output root hash.
    pub prev_output_root: B256,
    /// The config hash.
    pub config_hash: B256,
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec;

    use alloy_primitives::b256;

    use super::*;

    #[cfg(feature = "serde")]
    fn sample_proposal() -> Proposal {
        Proposal {
            output_root: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: b256!(
                "0000000000000000000000000000000000000000000000000000000000000002"
            ),
            l1_origin_number: U256::from(100),
            l2_block_number: U256::from(12345),
            prev_output_root: b256!(
                "0000000000000000000000000000000000000000000000000000000000000003"
            ),
            config_hash: b256!("0000000000000000000000000000000000000000000000000000000000000004"),
        }
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_proposal_serialization_snake_case() {
        let proposal = sample_proposal();
        let json = serde_json::to_string(&proposal).unwrap();

        assert!(json.contains("\"output_root\""));
        assert!(json.contains("\"signature\""));
        assert!(json.contains("\"l1_origin_hash\""));
        assert!(json.contains("\"l2_block_number\""));
        assert!(json.contains("\"prev_output_root\""));
        assert!(json.contains("\"config_hash\""));
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_proposal_hex_encoding() {
        let proposal = sample_proposal();
        let json = serde_json::to_string(&proposal).unwrap();

        assert!(json.contains("\"0x"));
        // 12345 = 0x3039
        assert!(json.contains("\"0x3039\""));
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_proposal_roundtrip() {
        let original = sample_proposal();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_proposal_l2_block_number_zero() {
        let mut proposal = sample_proposal();
        proposal.l2_block_number = U256::ZERO;

        let json = serde_json::to_string(&proposal).unwrap();
        assert!(json.contains("\"l2_block_number\":\"0x0\""));
    }
}
