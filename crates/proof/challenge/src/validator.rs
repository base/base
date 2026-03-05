//! Output root validation for candidate dispute games.
//!
//! The [`OutputValidator`] verifies both the final output root and intermediate
//! output roots for each [`CandidateGame`]. It fetches L2 block headers and
//! `L2ToL1MessagePasser` storage proofs, recomputes expected output roots using
//! [`output_root_v0`](base_enclave::output_root_v0), and compares them against
//! the on-chain claims.

use std::{sync::Arc, time::Instant};

use alloy_primitives::{Address, B256, address};
use base_enclave::output_root_v0;
use base_proof_rpc::{L2Provider, RpcError};
use thiserror::Error;
use tracing::{info, warn};

use crate::ChallengerMetrics;

/// Well-known address of the `L2ToL1MessagePasser` predeploy.
const L2_TO_L1_MESSAGE_PASSER: Address = address!("4200000000000000000000000000000000000016");

/// Errors that can occur during output root validation.
#[derive(Debug, Error)]
pub enum ValidatorError {
    /// The requested L2 block has not been produced yet.
    #[error("L2 block {block_number} is not yet available")]
    BlockNotAvailable {
        /// The block number that was requested.
        block_number: u64,
    },

    /// The RPC header hash does not match the computed consensus header hash.
    #[error(
        "header hash mismatch at block {block_number}: rpc={rpc_hash}, computed={computed_hash}"
    )]
    HeaderHashMismatch {
        /// The block number where the mismatch occurred.
        block_number: u64,
        /// The hash returned by the RPC node.
        rpc_hash: B256,
        /// The hash computed from the consensus header.
        computed_hash: B256,
    },

    /// The intermediate block interval is zero, which would cause an infinite loop.
    #[error("intermediate block interval must be non-zero")]
    InvalidInterval,

    /// The number of intermediate roots does not match the expected checkpoint count.
    #[error("checkpoint count mismatch: expected {expected}, got {actual}")]
    CheckpointCountMismatch {
        /// The expected number of checkpoints.
        expected: usize,
        /// The actual number of intermediate roots provided.
        actual: usize,
    },

    /// Arithmetic overflow in checkpoint calculation (adversarial on-chain values).
    #[error("arithmetic overflow in checkpoint calculation at block {block_number}")]
    ArithmeticOverflow {
        /// The block number where the overflow occurred.
        block_number: u64,
    },

    /// An RPC error occurred while fetching data from the L2 node.
    #[error("L2 RPC error: {0}")]
    Rpc(#[from] RpcError),
}

/// Result of validating a dispute game's output roots.
///
/// For final root validation, `expected_root` is the output root computed from
/// the L2 state. For intermediate root validation, `expected_root` is the
/// computed root at the first failing checkpoint when `is_valid` is `false`, or
/// equal to `claimed_root` when all checkpoints pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationResult {
    /// Whether all validated output roots match their expected values.
    pub is_valid: bool,
    /// The expected output root computed from the L2 state.
    ///
    /// For final root validation this is the root at the game's L2 block. For
    /// intermediate validation this is the computed root at the first invalid
    /// checkpoint, or `claimed_root` when all checkpoints are valid.
    pub expected_root: B256,
    /// The root claim from the on-chain game.
    pub claimed_root: B256,
    /// The index of the first invalid intermediate root, if any.
    pub invalid_intermediate_index: Option<usize>,
}

/// Validates output roots for candidate dispute games.
///
/// Fetches L2 block headers and `L2ToL1MessagePasser` storage proofs to
/// recompute expected output roots and compare them against on-chain claims.
pub struct OutputValidator<L2: L2Provider> {
    l2_provider: Arc<L2>,
}

impl<L2: L2Provider> std::fmt::Debug for OutputValidator<L2> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputValidator").finish_non_exhaustive()
    }
}

impl<L2: L2Provider> OutputValidator<L2> {
    /// Creates a new output validator.
    pub const fn new(l2_provider: Arc<L2>) -> Self {
        Self { l2_provider }
    }

    /// Computes the expected output root for a given L2 block number.
    ///
    /// Fetches the block header and `L2ToL1MessagePasser` storage proof, then
    /// passes them to [`output_root_v0`]. Verifies that the RPC-provided header
    /// hash matches the hash computed from the consensus header as a
    /// defense-in-depth check against compromised or buggy RPC nodes.
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError> {
        let rpc_header =
            self.l2_provider.header_by_number(Some(block_number)).await.map_err(|e| match &e {
                RpcError::HeaderNotFound(_) | RpcError::BlockNotFound(_) => {
                    ValidatorError::BlockNotAvailable { block_number }
                }
                _ => ValidatorError::Rpc(e),
            })?;

        let rpc_hash = rpc_header.hash;
        let consensus_header = rpc_header.inner;

        // Verify that the RPC-provided hash matches the actual consensus header
        // hash. This guards against a compromised or buggy RPC node returning a
        // header whose hash does not match the inner consensus data.
        let computed_hash = consensus_header.hash_slow();
        if rpc_hash != computed_hash {
            return Err(ValidatorError::HeaderHashMismatch {
                block_number,
                rpc_hash,
                computed_hash,
            });
        }

        let account_result = self.l2_provider.get_proof(L2_TO_L1_MESSAGE_PASSER, rpc_hash).await?;

        let storage_root = account_result.storage_hash;

        Ok(output_root_v0(&consensus_header, storage_root))
    }

    /// Validates the final output root of a candidate dispute game.
    ///
    /// Fetches the L2 header and `L2ToL1MessagePasser` storage proof at the
    /// game's L2 block number, computes the expected output root, and compares
    /// it against the game's `rootClaim`.
    pub async fn validate_final_root(
        &self,
        game_address: Address,
        l2_block_number: u64,
        claimed_root: B256,
    ) -> Result<ValidationResult, ValidatorError> {
        let start = Instant::now();

        info!(
            game = %game_address,
            block = l2_block_number,
            "validating final output root"
        );

        let expected_root = self.compute_output_root(l2_block_number).await?;
        let is_valid = expected_root == claimed_root;

        let elapsed = start.elapsed().as_secs_f64();
        metrics::histogram!(ChallengerMetrics::VALIDATION_LATENCY_SECONDS).record(elapsed);

        if !is_valid {
            warn!(
                game = %game_address,
                expected = %expected_root,
                claimed = %claimed_root,
                block = l2_block_number,
                "invalid final output root detected"
            );
            metrics::counter!(ChallengerMetrics::GAMES_INVALID_TOTAL).increment(1);
        }

        Ok(ValidationResult {
            is_valid,
            expected_root,
            claimed_root,
            invalid_intermediate_index: None,
        })
    }

    /// Validates the intermediate output roots of a candidate dispute game.
    ///
    /// Iterates checkpoint blocks from `starting_block_number + interval` to
    /// `l2_block_number` stepping by `interval`, computes the expected output
    /// root at each checkpoint, and compares them against the on-chain
    /// intermediate roots.
    ///
    /// Returns a [`ValidationResult`] where `invalid_intermediate_index`
    /// contains the index of the first mismatched intermediate root, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ValidatorError::InvalidInterval`] if `intermediate_block_interval`
    /// is zero, [`ValidatorError::CheckpointCountMismatch`] if the provided
    /// `intermediate_roots` length does not match the expected checkpoint count,
    /// or [`ValidatorError::ArithmeticOverflow`] if checkpoint arithmetic overflows
    /// (possible with adversarial on-chain values).
    pub async fn validate_intermediate_roots(
        &self,
        game_address: Address,
        starting_block_number: u64,
        l2_block_number: u64,
        intermediate_block_interval: u64,
        claimed_root: B256,
        intermediate_roots: &[B256],
    ) -> Result<ValidationResult, ValidatorError> {
        let start = Instant::now();

        if intermediate_block_interval == 0 {
            return Err(ValidatorError::InvalidInterval);
        }

        // Compute expected checkpoint count so we can verify intermediate_roots
        // covers every required block.
        let span = l2_block_number.saturating_sub(starting_block_number);
        let expected_count = (span / intermediate_block_interval) as usize;

        if intermediate_roots.len() != expected_count {
            return Err(ValidatorError::CheckpointCountMismatch {
                expected: expected_count,
                actual: intermediate_roots.len(),
            });
        }

        info!(
            game = %game_address,
            starting_block = starting_block_number,
            end_block = l2_block_number,
            interval = intermediate_block_interval,
            intermediate_count = intermediate_roots.len(),
            "validating intermediate output roots"
        );

        let mut first_invalid: Option<(usize, B256)> = None;

        let mut checkpoint = starting_block_number
            .checked_add(intermediate_block_interval)
            .ok_or(ValidatorError::ArithmeticOverflow { block_number: starting_block_number })?;
        let mut idx = 0;

        while checkpoint <= l2_block_number {
            if idx >= intermediate_roots.len() {
                break;
            }

            let expected_root = self.compute_output_root(checkpoint).await?;
            let claimed_intermediate = intermediate_roots[idx];

            if expected_root != claimed_intermediate {
                warn!(
                    game = %game_address,
                    index = idx,
                    block = checkpoint,
                    expected = %expected_root,
                    claimed = %claimed_intermediate,
                    "invalid intermediate output root detected"
                );
                first_invalid = Some((idx, expected_root));
                break;
            }

            checkpoint = checkpoint
                .checked_add(intermediate_block_interval)
                .ok_or(ValidatorError::ArithmeticOverflow { block_number: checkpoint })?;
            idx += 1;
        }

        let elapsed = start.elapsed().as_secs_f64();
        metrics::histogram!(ChallengerMetrics::VALIDATION_LATENCY_SECONDS).record(elapsed);

        let is_valid = first_invalid.is_none();
        if !is_valid {
            metrics::counter!(ChallengerMetrics::GAMES_INVALID_TOTAL).increment(1);
        }

        let expected_root = first_invalid.map_or(claimed_root, |(_, root)| root);
        let invalid_intermediate_index = first_invalid.map(|(idx, _)| idx);

        Ok(ValidationResult { is_valid, expected_root, claimed_root, invalid_intermediate_index })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_rpc_types_eth::Header as RpcHeader;
    use base_enclave::{AccountResult, output_root_v0};

    use super::*;
    use crate::test_utils::MockL2Provider;

    /// Creates a test consensus header for a given block number.
    fn test_header(block_number: u64) -> ConsensusHeader {
        ConsensusHeader {
            number: block_number,
            state_root: B256::repeat_byte(block_number as u8),
            ..Default::default()
        }
    }

    /// Creates a test account result with a given storage hash.
    fn test_account_result(storage_hash: B256) -> AccountResult {
        AccountResult {
            address: address!("4200000000000000000000000000000000000016"),
            account_proof: vec![Bytes::from(vec![0xab])],
            balance: U256::ZERO,
            code_hash: B256::ZERO,
            nonce: U256::ZERO,
            storage_hash,
            storage_proof: vec![],
        }
    }

    /// Creates a mock L2 provider with a single block and returns the expected
    /// output root for that block.
    fn mock_with_block(block_number: u64) -> (MockL2Provider, B256) {
        let header = test_header(block_number);
        let storage_hash = B256::repeat_byte(0xBB);
        let account = test_account_result(storage_hash);
        let expected_root = output_root_v0(&header, storage_hash);

        let mut provider = MockL2Provider::new();
        provider.insert_block(block_number, header, account);

        (provider, expected_root)
    }

    /// Creates a mock L2 provider with multiple blocks and returns a vec of
    /// expected output roots (one per block).
    fn mock_with_blocks(block_numbers: &[u64]) -> (MockL2Provider, Vec<B256>) {
        let mut provider = MockL2Provider::new();
        let mut roots = Vec::new();

        for &block_number in block_numbers {
            let header = test_header(block_number);
            let storage_hash = B256::repeat_byte(0xBB);
            let account = test_account_result(storage_hash);
            let expected_root = output_root_v0(&header, storage_hash);

            provider.insert_block(block_number, header, account);
            roots.push(expected_root);
        }

        (provider, roots)
    }

    /// Valid final root: the on-chain root claim matches the expected output root.
    #[tokio::test]
    async fn test_validate_final_root_valid() {
        let (provider, expected_root) = mock_with_block(100);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x01);

        let result = validator.validate_final_root(game_address, 100, expected_root).await.unwrap();

        assert!(result.is_valid);
        assert_eq!(result.expected_root, expected_root);
        assert_eq!(result.claimed_root, expected_root);
        assert_eq!(result.invalid_intermediate_index, None);
    }

    /// Invalid final root: the on-chain root claim does NOT match the expected output root.
    #[tokio::test]
    async fn test_validate_final_root_invalid() {
        let (provider, expected_root) = mock_with_block(100);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x02);

        // Provide a wrong root claim
        let wrong_root = B256::repeat_byte(0xFF);

        let result = validator.validate_final_root(game_address, 100, wrong_root).await.unwrap();

        assert!(!result.is_valid);
        assert_eq!(result.expected_root, expected_root);
        assert_eq!(result.claimed_root, wrong_root);
        assert_eq!(result.invalid_intermediate_index, None);
    }

    /// Valid intermediate roots: all checkpoints match expected output roots.
    #[tokio::test]
    async fn test_validate_intermediate_roots_valid() {
        // starting_block = 90, l2_block = 100, interval = 5
        // Checkpoint blocks: 95, 100
        let final_claimed = B256::repeat_byte(0xAA);
        let (provider, roots) = mock_with_blocks(&[95, 100]);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x03);

        let result = validator
            .validate_intermediate_roots(game_address, 90, 100, 5, final_claimed, &roots)
            .await
            .unwrap();

        assert!(result.is_valid);
        assert_eq!(result.invalid_intermediate_index, None);
        // When all intermediates are valid, expected_root equals claimed_root
        assert_eq!(result.expected_root, final_claimed);
    }

    /// Invalid intermediate root: the second checkpoint does not match.
    #[tokio::test]
    async fn test_validate_intermediate_roots_invalid() {
        // starting_block = 90, l2_block = 100, interval = 5
        // Checkpoint blocks: 95, 100
        let (provider, mut roots) = mock_with_blocks(&[95, 100]);
        // Save the correct root at block 100 before corrupting
        let correct_root_at_100 = roots[1];
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x04);

        // Corrupt the second intermediate root
        roots[1] = B256::repeat_byte(0xFF);

        let result = validator
            .validate_intermediate_roots(game_address, 90, 100, 5, B256::ZERO, &roots)
            .await
            .unwrap();

        assert!(!result.is_valid);
        assert_eq!(result.invalid_intermediate_index, Some(1));
        // expected_root is the computed root at the first failing checkpoint
        assert_eq!(result.expected_root, correct_root_at_100);
    }

    /// Final root is valid but intermediate root is invalid.
    #[tokio::test]
    async fn test_final_valid_intermediate_invalid() {
        // Block 100 is the final block, block 95 is intermediate checkpoint
        let (provider, roots) = mock_with_blocks(&[95, 100]);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x05);

        // Final root validation succeeds
        let final_result =
            validator.validate_final_root(game_address, 100, roots[1]).await.unwrap();
        assert!(final_result.is_valid);

        // Intermediate root validation fails (corrupt index 0)
        let mut intermediate_roots = roots.clone();
        intermediate_roots[0] = B256::repeat_byte(0xDD);

        let intermediate_result = validator
            .validate_intermediate_roots(game_address, 90, 100, 5, roots[1], &intermediate_roots)
            .await
            .unwrap();

        assert!(!intermediate_result.is_valid);
        assert_eq!(intermediate_result.invalid_intermediate_index, Some(0));
        // expected_root is the computed root at checkpoint block 95 (the first failing index)
        assert_eq!(intermediate_result.expected_root, roots[0]);
    }

    /// Missing L2 block: returns `ValidatorError::BlockNotAvailable` instead of panicking.
    #[tokio::test]
    async fn test_missing_l2_block() {
        let mut provider = MockL2Provider::new();
        // Mark block 100 as an error block (not yet produced)
        provider.error_blocks.push(100);

        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x06);

        let result = validator.validate_final_root(game_address, 100, B256::ZERO).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ValidatorError::BlockNotAvailable { block_number: 100 }),
            "expected BlockNotAvailable, got: {err:?}"
        );
    }

    /// Zero intermediate block interval returns `ValidatorError::InvalidInterval`.
    #[tokio::test]
    async fn test_zero_intermediate_interval() {
        let provider = MockL2Provider::new();
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x07);

        let result =
            validator.validate_intermediate_roots(game_address, 90, 100, 0, B256::ZERO, &[]).await;

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ValidatorError::InvalidInterval),
            "expected InvalidInterval"
        );
    }

    /// Checkpoint count mismatch: fewer intermediate roots than expected checkpoints.
    #[tokio::test]
    async fn test_checkpoint_count_mismatch_too_few() {
        let provider = MockL2Provider::new();
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x08);

        // starting=90, end=100, interval=5 -> expected 2 checkpoints (95, 100)
        // but only provide 1 root
        let result = validator
            .validate_intermediate_roots(
                game_address,
                90,
                100,
                5,
                B256::ZERO,
                &[B256::ZERO], // only 1, need 2
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ValidatorError::CheckpointCountMismatch { expected: 2, actual: 1 }),
            "expected CheckpointCountMismatch {{ expected: 2, actual: 1 }}, got: {err:?}"
        );
    }

    /// Checkpoint count mismatch: more intermediate roots than expected checkpoints.
    #[tokio::test]
    async fn test_checkpoint_count_mismatch_too_many() {
        let provider = MockL2Provider::new();
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x09);

        // starting=90, end=100, interval=5 -> expected 2 checkpoints (95, 100)
        // but provide 3 roots
        let result = validator
            .validate_intermediate_roots(
                game_address,
                90,
                100,
                5,
                B256::ZERO,
                &[B256::ZERO, B256::ZERO, B256::ZERO], // 3, need 2
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ValidatorError::CheckpointCountMismatch { expected: 2, actual: 3 }),
            "expected CheckpointCountMismatch {{ expected: 2, actual: 3 }}, got: {err:?}"
        );
    }

    /// Arithmetic overflow in checkpoint calculation returns error.
    #[tokio::test]
    async fn test_arithmetic_overflow() {
        let (provider, roots) = mock_with_blocks(&[]);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0A);

        // starting_block near u64::MAX with interval that causes overflow
        // span = l2_block_number - starting_block_number = u64::MAX - (u64::MAX - 1) = 1
        // expected_count = 1 / u64::MAX = 0, so pass empty roots
        let result = validator
            .validate_intermediate_roots(
                game_address,
                u64::MAX - 1,
                u64::MAX,
                u64::MAX, // This would overflow: (u64::MAX-1) + u64::MAX
                B256::ZERO,
                &roots,
            )
            .await;

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ValidatorError::ArithmeticOverflow { .. }),
            "expected ArithmeticOverflow"
        );
    }

    /// Header hash mismatch: RPC returns a header whose hash does not match the
    /// consensus header, and the validator rejects it.
    #[tokio::test]
    async fn test_header_hash_mismatch() {
        let consensus_header = test_header(100);
        let storage_hash = B256::repeat_byte(0xBB);
        let account = test_account_result(storage_hash);
        let correct_hash = consensus_header.hash_slow();

        let mut provider = MockL2Provider::new();
        // Insert block normally first, then tamper with the header hash
        provider.insert_block(100, consensus_header.clone(), account);
        // Overwrite the header with a mismatched hash
        let tampered_header = RpcHeader {
            hash: B256::repeat_byte(0xEE),
            inner: consensus_header,
            ..Default::default()
        };
        provider.headers.insert(100, tampered_header);

        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0B);

        let result = validator.validate_final_root(game_address, 100, B256::ZERO).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ValidatorError::HeaderHashMismatch { block_number, rpc_hash, computed_hash } => {
                assert_eq!(block_number, 100);
                assert_eq!(rpc_hash, B256::repeat_byte(0xEE));
                assert_eq!(computed_hash, correct_hash);
            }
            other => panic!("expected HeaderHashMismatch, got: {other:?}"),
        }
    }
}
