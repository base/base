//! Output root computation utilities.
//!
//! This module provides functions to compute output roots for L2 blocks,
//! matching the Go implementation in `server.go`.

use alloy_consensus::Header;
use alloy_primitives::{B256, keccak256};

/// Compute output root v0 (matches Go's `OutputRootV0` in server.go:347-354).
///
/// The output root is computed as:
/// ```text
/// keccak256(version || state_root || storage_root || block_hash)
/// ```
///
/// Where:
/// - `version` is 32 zero bytes (version 0)
/// - `state_root` is the state root from the block header (32 bytes)
/// - `storage_root` is the storage root of the `L2ToL1MessagePasser` contract (32 bytes)
/// - `block_hash` is the hash of the block header (32 bytes)
///
/// # Arguments
///
/// * `header` - The L2 block header
/// * `storage_root` - The storage root of the `L2ToL1MessagePasser` contract
///
/// # Returns
///
/// The 32-byte output root hash.
#[must_use]
pub fn output_root_v0(header: &Header, storage_root: B256) -> B256 {
    let block_hash = header.hash_slow();
    let state_root = header.state_root;

    // 128 bytes: version (32, all zeros) || state_root (32) || storage_root (32) || block_hash (32)
    let mut buf = [0u8; 128];
    // buf[0..32] = version (already zeros)
    buf[32..64].copy_from_slice(state_root.as_slice());
    buf[64..96].copy_from_slice(storage_root.as_slice());
    buf[96..128].copy_from_slice(block_hash.as_slice());

    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;

    /// Creates a test header with known values for deterministic output.
    fn test_header_for_output() -> Header {
        Header {
            parent_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
            beneficiary: Default::default(),
            state_root: b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            transactions_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            receipts_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            logs_bloom: Default::default(),
            difficulty: Default::default(),
            number: 1,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: 1_700_000_000,
            extra_data: Default::default(),
            mix_hash: Default::default(),
            nonce: Default::default(),
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        }
    }

    #[test]
    fn test_output_root_v0_structure() {
        let header = test_header_for_output();
        let storage_root =
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let output_root = output_root_v0(&header, storage_root);

        // Output root should be 32 bytes (B256)
        assert_eq!(output_root.len(), 32);

        // Same inputs should produce same output
        let output_root_2 = output_root_v0(&header, storage_root);
        assert_eq!(output_root, output_root_2);
    }

    #[test]
    fn test_output_root_v0_different_storage_roots() {
        let header = test_header_for_output();
        let storage_root_1 =
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let storage_root_2 =
            b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");

        let output_root_1 = output_root_v0(&header, storage_root_1);
        let output_root_2 = output_root_v0(&header, storage_root_2);

        // Different storage roots should produce different output roots
        assert_ne!(output_root_1, output_root_2);
    }

    #[test]
    fn test_output_root_v0_different_state_roots() {
        let header_1 = test_header_for_output();
        let mut header_2 = test_header_for_output();
        header_2.state_root =
            b256!("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");

        let storage_root =
            b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let output_root_1 = output_root_v0(&header_1, storage_root);
        let output_root_2 = output_root_v0(&header_2, storage_root);

        // Different state roots should produce different output roots
        assert_ne!(output_root_1, output_root_2);
    }
}
