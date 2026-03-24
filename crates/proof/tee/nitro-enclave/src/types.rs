//! Enclave-native proof types.
//!
//! These types mirror their counterparts in `base-proof-primitives` but are
//! owned by the enclave crate so that changes to the host-side types (e.g.
//! adding fields to `ProofRequest`) do not alter the enclave binary and
//! therefore do not change the PCR0 measurement.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, U256};

/// ECDSA signature length in bytes (r: 32 + s: 32 + v: 1).
pub const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Base length of the proof journal without intermediate roots:
/// address(20) + 7 × bytes32(32) = 244 bytes.
pub const PROOF_JOURNAL_BASE_LENGTH: usize = 244;

/// The `AggregateVerifier` contract journal encoding.
///
/// Serializes proposal fields into the byte format expected by on-chain verification:
///
/// ```text
/// prover(20) || l1OriginHash(32) || prevOutputRoot(32)
///   || startingL2Block(32) || outputRoot(32) || endingL2Block(32)
///   || intermediateRoots(32*N) || configHash(32) || imageHash(32)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofJournal {
    /// The proposer address.
    pub proposer: Address,
    /// The L1 origin block hash.
    pub l1_origin_hash: B256,
    /// The previous output root hash.
    pub prev_output_root: B256,
    /// The starting L2 block number.
    pub starting_l2_block: U256,
    /// The output root hash.
    pub output_root: B256,
    /// The ending L2 block number.
    pub ending_l2_block: U256,
    /// Intermediate output roots for aggregate proposals.
    pub intermediate_roots: Vec<B256>,
    /// The config hash.
    pub config_hash: B256,
    /// The TEE image hash.
    pub tee_image_hash: B256,
}

impl ProofJournal {
    /// Encode the journal into the ABI-packed byte format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut data =
            Vec::with_capacity(PROOF_JOURNAL_BASE_LENGTH + 32 * self.intermediate_roots.len());

        data.extend_from_slice(self.proposer.as_slice());
        data.extend_from_slice(self.l1_origin_hash.as_slice());
        data.extend_from_slice(self.prev_output_root.as_slice());
        data.extend_from_slice(&self.starting_l2_block.to_be_bytes::<32>());
        data.extend_from_slice(self.output_root.as_slice());
        data.extend_from_slice(&self.ending_l2_block.to_be_bytes::<32>());
        for root in &self.intermediate_roots {
            data.extend_from_slice(root.as_slice());
        }
        data.extend_from_slice(self.config_hash.as_slice());
        data.extend_from_slice(self.tee_image_hash.as_slice());

        data
    }
}

/// A proposal containing an output root and signature.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Result of a TEE proof computation.
///
/// This is the enclave-native equivalent of `ProofResult::Tee` from
/// `base-proof-primitives`, but as a flat struct rather than an enum variant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TeeProofResult {
    /// The aggregated proposal covering the entire proven block range.
    pub aggregate_proposal: Proposal,
    /// The individual per-block proposals that were aggregated.
    pub proposals: Vec<Proposal>,
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{address, b256};

    use super::*;

    fn test_journal() -> ProofJournal {
        ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: b256!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ),
            prev_output_root: b256!(
                "3333333333333333333333333333333333333333333333333333333333333333"
            ),
            starting_l2_block: U256::from(999),
            output_root: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
            ending_l2_block: U256::from(1000),
            intermediate_roots: vec![],
            config_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            tee_image_hash: b256!(
                "5555555555555555555555555555555555555555555555555555555555555555"
            ),
        }
    }

    #[test]
    fn test_journal_encode_length() {
        let data = test_journal().encode();
        assert_eq!(data.len(), PROOF_JOURNAL_BASE_LENGTH);
        assert_eq!(data.len(), 244);
    }

    #[test]
    fn test_journal_encode_components() {
        let journal = test_journal();
        let data = journal.encode();

        let mut off = 0;
        assert_eq!(
            &data[off..off + 20],
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266").as_slice()
        );
        off += 20;
        assert_eq!(&data[off..off + 32], journal.l1_origin_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.prev_output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &journal.starting_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.output_root.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], &journal.ending_l2_block.to_be_bytes::<32>());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.config_hash.as_slice());
        off += 32;
        assert_eq!(&data[off..off + 32], journal.tee_image_hash.as_slice());
    }

    #[test]
    fn test_journal_encode_with_intermediate_roots() {
        let journal = ProofJournal {
            proposer: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            l1_origin_hash: B256::ZERO,
            prev_output_root: B256::ZERO,
            starting_l2_block: U256::ZERO,
            output_root: B256::ZERO,
            ending_l2_block: U256::ZERO,
            intermediate_roots: vec![
                b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
            config_hash: B256::ZERO,
            tee_image_hash: B256::ZERO,
        };

        let data = journal.encode();
        assert_eq!(data.len(), PROOF_JOURNAL_BASE_LENGTH + 64);

        let ir_offset = 20 + 5 * 32;
        assert_eq!(&data[ir_offset..ir_offset + 32], journal.intermediate_roots[0].as_slice());
        assert_eq!(&data[ir_offset + 32..ir_offset + 64], journal.intermediate_roots[1].as_slice());
    }
}
