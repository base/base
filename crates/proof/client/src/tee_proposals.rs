use alloc::vec::Vec;

use alloy_primitives::{B256, Bytes};
use base_proof::BootInfo;
use base_proof_primitives::{ProofJournal, ProofResult, Proposal};
use base_protocol::L2BlockInfo;

/// Builds signed TEE proposals from proof-client block results.
#[derive(Debug)]
pub struct TeeProposals;

impl TeeProposals {
    /// Error message used when the proof driver returns no block results.
    pub const EMPTY_PROPOSALS_ERROR: &'static str = "no proposals produced";

    /// Error message used when an L2 block number cannot be decremented.
    pub const L2_BLOCK_NUMBER_ZERO_ERROR: &'static str = "l2_block_number is 0";

    /// Error message used when aggregate checkpoint sampling has a zero interval.
    pub const INTERMEDIATE_BLOCK_INTERVAL_ZERO_ERROR: &'static str =
        "intermediate_block_interval must not be zero";

    /// Build per-block TEE proposals and the aggregate proposal.
    ///
    /// # Errors
    ///
    /// Returns `proof_error` for invalid proposal inputs, or the signer error returned by `sign`.
    pub fn build<E>(
        boot_info: &BootInfo,
        block_results: &[(L2BlockInfo, B256)],
        config_hash: B256,
        tee_image_hash: B256,
        mut sign: impl FnMut(&[u8]) -> Result<Bytes, E>,
        proof_error: impl Fn(&'static str) -> E,
    ) -> Result<ProofResult, E> {
        if block_results.is_empty() {
            return Err(proof_error(Self::EMPTY_PROPOSALS_ERROR));
        }

        let agreed_l2_output_root = boot_info.agreed_l2_output_root;
        let l1_origin_hash = boot_info.l1_head;
        let l1_origin_number = boot_info.l1_head_number;

        let mut sign_proposal = |journal: ProofJournal,
                                 output_root: B256,
                                 l2_block_number: u64,
                                 prev_output_root: B256|
         -> Result<Proposal, E> {
            Ok(Proposal {
                output_root,
                signature: sign(journal.encode().as_slice())?,
                l1_origin_hash,
                l1_origin_number,
                l2_block_number,
                prev_output_root,
                config_hash,
            })
        };

        let mut proposals = Vec::with_capacity(block_results.len());
        let mut prev_output_root = agreed_l2_output_root;

        for (l2_info, output_root) in block_results {
            let l2_block_number = l2_info.block_info.number;
            let starting_l2_block = l2_block_number
                .checked_sub(1)
                .ok_or_else(|| proof_error(Self::L2_BLOCK_NUMBER_ZERO_ERROR))?;
            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root,
                starting_l2_block,
                output_root: *output_root,
                ending_l2_block: l2_block_number,
                intermediate_roots: Vec::new(),
                config_hash,
                tee_image_hash,
            };

            proposals.push(sign_proposal(
                journal,
                *output_root,
                l2_block_number,
                prev_output_root,
            )?);
            prev_output_root = *output_root;
        }

        let aggregate_proposal = if proposals.len() == 1 {
            proposals[0].clone()
        } else {
            let first = &proposals[0];
            let last = proposals.last().expect("checked non-empty proposals");

            let interval = boot_info.intermediate_block_interval;
            if interval == 0 {
                return Err(proof_error(Self::INTERMEDIATE_BLOCK_INTERVAL_ZERO_ERROR));
            }
            let interval = interval as usize;
            let intermediate_roots = proposals
                .chunks_exact(interval)
                .map(|chunk| chunk[interval - 1].output_root)
                .collect();

            let starting_l2_block = first
                .l2_block_number
                .checked_sub(1)
                .ok_or_else(|| proof_error(Self::L2_BLOCK_NUMBER_ZERO_ERROR))?;
            let journal = ProofJournal {
                proposer: boot_info.proposer,
                l1_origin_hash,
                prev_output_root: agreed_l2_output_root,
                starting_l2_block,
                output_root: last.output_root,
                ending_l2_block: last.l2_block_number,
                intermediate_roots,
                config_hash,
                tee_image_hash,
            };

            sign_proposal(journal, last.output_root, last.l2_block_number, agreed_l2_output_root)?
        };

        Ok(ProofResult::Tee { aggregate_proposal, proposals })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_genesis::ChainConfig;
    use alloy_primitives::{Address, b256};
    use base_common_genesis::RollupConfig;
    use base_protocol::BlockInfo;

    use super::*;

    fn boot_info(interval: u64) -> BootInfo {
        BootInfo {
            l1_head: b256!("0101010101010101010101010101010101010101010101010101010101010101"),
            agreed_l2_output_root: b256!(
                "0202020202020202020202020202020202020202020202020202020202020202"
            ),
            claimed_l2_output_root: b256!(
                "0303030303030303030303030303030303030303030303030303030303030303"
            ),
            claimed_l2_block_number: 3,
            chain_id: 8453,
            activation_admin_address: None,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
            proposer: Address::repeat_byte(4),
            intermediate_block_interval: interval,
            l1_head_number: 12,
        }
    }

    fn l2_info(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo::new(B256::repeat_byte(number as u8), number, B256::ZERO, 0),
            ..Default::default()
        }
    }

    #[test]
    fn build_signs_per_block_and_aggregate_proposals() {
        let roots = [
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            b256!("3333333333333333333333333333333333333333333333333333333333333333"),
        ];
        let block_results =
            vec![(l2_info(1), roots[0]), (l2_info(2), roots[1]), (l2_info(3), roots[2])];
        let mut signing_calls = 0u8;

        let proof = TeeProposals::build(
            &boot_info(2),
            &block_results,
            B256::repeat_byte(5),
            B256::repeat_byte(6),
            |_| {
                signing_calls += 1;
                Ok(Bytes::from(vec![signing_calls; 65]))
            },
            |message| message,
        )
        .unwrap();

        let ProofResult::Tee { aggregate_proposal, proposals } = proof else {
            panic!("expected TEE proof");
        };
        assert_eq!(proposals.len(), 3);
        assert_eq!(aggregate_proposal.output_root, roots[2]);
        assert_eq!(aggregate_proposal.prev_output_root, boot_info(2).agreed_l2_output_root);
        assert_eq!(aggregate_proposal.signature, Bytes::from(vec![4; 65]));
    }

    #[test]
    fn build_rejects_zero_aggregate_interval() {
        let block_results =
            vec![(l2_info(1), B256::repeat_byte(1)), (l2_info(2), B256::repeat_byte(2))];

        let err = TeeProposals::build(
            &boot_info(0),
            &block_results,
            B256::ZERO,
            B256::ZERO,
            |_| Ok(Bytes::new()),
            |message| message,
        )
        .unwrap_err();

        assert_eq!(err, TeeProposals::INTERMEDIATE_BLOCK_INTERVAL_ZERO_ERROR);
    }
}
