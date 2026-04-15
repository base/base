//! Output root validation for candidate dispute games.
//!
//! The [`OutputValidator`] verifies both the final output root and intermediate
//! output roots for each [`CandidateGame`]. It fetches L2 block headers and
//! `L2ToL1MessagePasser` storage proofs, recomputes expected output roots using
//! [`OutputRoot`](base_protocol::OutputRoot), and compares them against the
//! onchain claims.

use std::sync::Arc;

use alloy_primitives::{Address, B256};
use base_common_consensus::Predeploys;
use base_proof_rpc::{L2Provider, RpcError};
use base_protocol::OutputRoot;
use futures::stream::{self, StreamExt};
use thiserror::Error;
use tracing::{info, warn};

use crate::{AccountProofVerifier, ChallengerMetrics};

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

    /// The block range is degenerate (`starting_block_number >= l2_block_number`).
    #[error(
        "invalid block range: starting block {starting_block_number} >= end block {l2_block_number}"
    )]
    InvalidBlockRange {
        /// The starting block number.
        starting_block_number: u64,
        /// The end block (L2 block number).
        l2_block_number: u64,
    },

    /// The number of intermediate roots does not match the expected checkpoint count.
    #[error("checkpoint count mismatch: expected {expected}, got {actual}")]
    CheckpointCountMismatch {
        /// The expected number of checkpoints.
        expected: usize,
        /// The actual number of intermediate roots provided.
        actual: usize,
    },

    /// Arithmetic overflow in checkpoint calculation (adversarial onchain values).
    #[error("arithmetic overflow in checkpoint calculation at block {block_number}")]
    ArithmeticOverflow {
        /// The block number where the overflow occurred.
        block_number: u64,
    },

    /// Account proof verification against the header state root failed.
    #[error("account proof verification failed at block {block_number}: {reason}")]
    AccountProofFailed {
        /// The block number where the proof verification failed.
        block_number: u64,
        /// The reason the proof verification failed.
        reason: String,
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
    /// The root claim from the onchain game.
    pub claimed_root: B256,
    /// The index of the first invalid intermediate root, if any.
    pub invalid_intermediate_index: Option<usize>,
}

/// Parameters for validating intermediate output roots of a dispute game.
#[derive(Debug)]
pub struct IntermediateValidationParams<'a> {
    /// The onchain address of the dispute game proxy contract.
    pub game_address: Address,
    /// The L2 block number at the start of this game's range.
    pub starting_block_number: u64,
    /// The L2 block number at the end of this game's range.
    pub l2_block_number: u64,
    /// The block interval between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// The final output root claimed by this dispute game.
    pub claimed_root: B256,
    /// The intermediate output roots to validate, one per checkpoint.
    pub intermediate_roots: &'a [B256],
}

/// A single output root checkpoint to validate.
struct Checkpoint {
    /// The L2 block number at this checkpoint.
    block: u64,
    /// The onchain-claimed output root at this block.
    claimed_root: B256,
}

/// Validates output roots for candidate dispute games.
///
/// Fetches L2 block headers and `L2ToL1MessagePasser` storage proofs to
/// recompute expected output roots and compare them against onchain claims.
pub struct OutputValidator<L2: L2Provider> {
    l2_provider: Arc<L2>,
}

impl<L2: L2Provider> std::fmt::Debug for OutputValidator<L2> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputValidator").finish_non_exhaustive()
    }
}

impl<L2: L2Provider> OutputValidator<L2> {
    /// Maximum number of intermediate output roots to validate concurrently.
    pub const VALIDATION_CONCURRENCY: usize = 32;

    /// Creates a new output validator.
    pub const fn new(l2_provider: Arc<L2>) -> Self {
        Self { l2_provider }
    }

    /// Computes the expected output root for a given L2 block number.
    ///
    /// Fetches the block header and `L2ToL1MessagePasser` storage proof, then
    /// passes them to [`OutputRoot`]. Verifies that the RPC-provided header
    /// hash matches the hash computed from the consensus header as a
    /// defense-in-depth check against compromised or buggy RPC nodes.
    pub async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError> {
        let (_, output_root) = self.compute_output_root_with_hash(block_number).await?;
        Ok(output_root)
    }

    /// Computes the output root and block hash for a given L2 block number.
    ///
    /// Also verifies that the RPC-provided header hash matches the hash
    /// computed from the consensus header, guarding against a compromised or
    /// buggy RPC node.
    pub async fn compute_output_root_with_hash(
        &self,
        block_number: u64,
    ) -> Result<(B256, B256), ValidatorError> {
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

        let account_result =
            self.l2_provider.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, rpc_hash).await?;

        AccountProofVerifier::verify(&account_result, consensus_header.state_root).map_err(
            |e| ValidatorError::AccountProofFailed { block_number, reason: e.to_string() },
        )?;

        let storage_root = account_result.storage_hash;
        let output_root =
            OutputRoot::from_parts(consensus_header.state_root, storage_root, computed_hash).hash();

        Ok((computed_hash, output_root))
    }

    /// Validates output roots at the given checkpoints.
    ///
    /// Returns `Ok(None)` when every checkpoint matches. Returns
    /// `Ok(Some((index, expected_root)))` for the first mismatch.
    /// Records latency, error, and invalid-game metrics internally.
    async fn validate_output_roots(
        &self,
        game_address: Address,
        checkpoints: &[Checkpoint],
    ) -> Result<Option<(usize, B256)>, ValidatorError> {
        let _latency = base_metrics::timed!(ChallengerMetrics::validation_latency_seconds());

        let mut stream = stream::iter(checkpoints.iter().enumerate())
            .map(|(idx, cp)| async move { (idx, cp, self.compute_output_root(cp.block).await) })
            .buffered(Self::VALIDATION_CONCURRENCY);

        while let Some((idx, cp, result)) = stream.next().await {
            let expected_root = result.inspect_err(|e| {
                ChallengerMetrics::validation_errors_total().increment(1);
                warn!(
                    game = %game_address,
                    block = cp.block,
                    index = idx,
                    error = %e,
                    "output root computation failed"
                );
            })?;

            if expected_root != cp.claimed_root {
                warn!(
                    game = %game_address,
                    block = cp.block,
                    index = idx,
                    expected = %expected_root,
                    claimed = %cp.claimed_root,
                    "invalid output root detected"
                );
                ChallengerMetrics::games_invalid_total().increment(1);
                return Ok(Some((idx, expected_root)));
            }
        }

        Ok(None)
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
        info!(game = %game_address, block = l2_block_number, "validating final output root");

        let checkpoint = Checkpoint { block: l2_block_number, claimed_root };
        let mismatch = self.validate_output_roots(game_address, &[checkpoint]).await?;
        let (is_valid, expected_root) = match mismatch {
            Some((_, expected)) => (false, expected),
            None => (true, claimed_root),
        };

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
    /// root at each checkpoint, and compares them against the onchain
    /// intermediate roots.
    ///
    /// Returns a [`ValidationResult`] where `invalid_intermediate_index`
    /// contains the index of the first mismatched intermediate root, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ValidatorError::InvalidInterval`] if `intermediate_block_interval`
    /// is zero, [`ValidatorError::InvalidBlockRange`] if
    /// `starting_block_number >= l2_block_number` (degenerate block range),
    /// [`ValidatorError::CheckpointCountMismatch`] if the provided
    /// `intermediate_roots` length does not match the expected checkpoint count,
    /// or [`ValidatorError::ArithmeticOverflow`] if checkpoint arithmetic overflows
    /// (possible with adversarial onchain values).
    pub async fn validate_intermediate_roots(
        &self,
        params: IntermediateValidationParams<'_>,
    ) -> Result<ValidationResult, ValidatorError> {
        let IntermediateValidationParams {
            game_address,
            starting_block_number,
            l2_block_number,
            intermediate_block_interval,
            claimed_root,
            intermediate_roots,
        } = params;

        if intermediate_block_interval == 0 {
            return Err(ValidatorError::InvalidInterval);
        }

        if starting_block_number >= l2_block_number {
            return Err(ValidatorError::InvalidBlockRange {
                starting_block_number,
                l2_block_number,
            });
        }

        // Compute expected checkpoint count so we can verify intermediate_roots
        // covers every required block.
        let span = l2_block_number.saturating_sub(starting_block_number);
        let expected_count = usize::try_from(span / intermediate_block_interval).map_err(|_| {
            ValidatorError::ArithmeticOverflow { block_number: starting_block_number }
        })?;

        if intermediate_roots.len() != expected_count {
            return Err(ValidatorError::CheckpointCountMismatch {
                expected: expected_count,
                actual: intermediate_roots.len(),
            });
        }

        if intermediate_roots.is_empty() {
            return Ok(ValidationResult {
                is_valid: true,
                expected_root: claimed_root,
                claimed_root,
                invalid_intermediate_index: None,
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

        let checkpoints = intermediate_roots
            .iter()
            .enumerate()
            .map(|(i, &root)| {
                let multiplier = u64::try_from(i + 1).map_err(|_| {
                    ValidatorError::ArithmeticOverflow { block_number: starting_block_number }
                })?;
                let offset = intermediate_block_interval.checked_mul(multiplier).ok_or(
                    ValidatorError::ArithmeticOverflow { block_number: starting_block_number },
                )?;
                let block = starting_block_number.checked_add(offset).ok_or(
                    ValidatorError::ArithmeticOverflow { block_number: starting_block_number },
                )?;
                Ok(Checkpoint { block, claimed_root: root })
            })
            .collect::<Result<Vec<_>, ValidatorError>>()?;

        let mismatch = self.validate_output_roots(game_address, &checkpoints).await?;
        let is_valid = mismatch.is_none();
        let expected_root = mismatch.as_ref().map_or(claimed_root, |(_, root)| *root);
        let invalid_intermediate_index = mismatch.map(|(idx, _)| idx);

        Ok(ValidationResult { is_valid, expected_root, claimed_root, invalid_intermediate_index })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_eth::Header as RpcHeader;
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{MockL2Provider, build_test_header_and_account};

    /// Creates a mock L2 provider with multiple blocks and returns a vec of
    /// expected output roots (one per block).
    fn mock_with_blocks(block_numbers: &[u64]) -> (MockL2Provider, Vec<B256>) {
        let mut provider = MockL2Provider::new();
        let mut roots = Vec::new();

        for &block_number in block_numbers {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, account) = build_test_header_and_account(block_number, storage_hash);
            let expected_root =
                OutputRoot::from_parts(header.state_root, storage_hash, header.hash_slow()).hash();

            provider.insert_block(block_number, header, account);
            roots.push(expected_root);
        }

        (provider, roots)
    }

    #[rstest]
    #[case::valid(None, true)]
    #[case::invalid(Some(B256::repeat_byte(0xFF)), false)]
    #[tokio::test]
    async fn test_validate_final_root(
        #[case] wrong_root: Option<B256>,
        #[case] expect_valid: bool,
    ) {
        let (provider, roots) = mock_with_blocks(&[100]);
        let expected_root = roots[0];
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x01);
        let claimed_root = wrong_root.unwrap_or(expected_root);

        let result = validator.validate_final_root(game_address, 100, claimed_root).await.unwrap();

        assert_eq!(result.is_valid, expect_valid);
        assert_eq!(result.expected_root, expected_root);
        assert_eq!(result.claimed_root, claimed_root);
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
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 5,
                claimed_root: final_claimed,
                intermediate_roots: &roots,
            })
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
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 5,
                claimed_root: B256::ZERO,
                intermediate_roots: &roots,
            })
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
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 5,
                claimed_root: roots[1],
                intermediate_roots: &intermediate_roots,
            })
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

        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 0,
                claimed_root: B256::ZERO,
                intermediate_roots: &[],
            })
            .await;

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ValidatorError::InvalidInterval),
            "expected InvalidInterval"
        );
    }

    #[rstest]
    #[case::too_few(vec![B256::ZERO], 2, 1)]
    #[case::too_many(vec![B256::ZERO; 3], 2, 3)]
    #[tokio::test]
    async fn test_checkpoint_count_mismatch(
        #[case] intermediate_roots: Vec<B256>,
        #[case] expected: usize,
        #[case] actual: usize,
    ) {
        let provider = MockL2Provider::new();
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x08);

        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 5,
                claimed_root: B256::ZERO,
                intermediate_roots: &intermediate_roots,
            })
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ValidatorError::CheckpointCountMismatch { expected: e, actual: a } if e == expected && a == actual),
            "expected CheckpointCountMismatch {{ expected: {expected}, actual: {actual} }}, got: {err:?}"
        );
    }

    /// When zero intermediate checkpoints are expected, the function should
    /// return a valid result even if the arithmetic would overflow.
    #[tokio::test]
    async fn test_arithmetic_overflow() {
        let (provider, roots) = mock_with_blocks(&[]);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0A);

        // starting_block near u64::MAX with interval that causes overflow
        // span = l2_block_number - starting_block_number = u64::MAX - (u64::MAX - 1) = 1
        // expected_count = 1 / u64::MAX = 0, so pass empty roots
        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: u64::MAX - 1,
                l2_block_number: u64::MAX,
                intermediate_block_interval: u64::MAX, // Would overflow: (u64::MAX-1) + u64::MAX, but no checkpoints needed
                claimed_root: B256::ZERO,
                intermediate_roots: &roots,
            })
            .await;

        let validation = result.expect("zero checkpoints should succeed without overflow");
        assert!(validation.is_valid);
        assert!(validation.invalid_intermediate_index.is_none());
    }

    /// Regression: when the final checkpoint equals `u64::MAX`, the loop should
    /// exit without attempting `checked_add` on the already-collected result.
    #[tokio::test]
    async fn test_no_overflow_when_all_checkpoints_collected() {
        let starting = u64::MAX - 20;
        let l2 = u64::MAX;
        let interval = 10;
        // Checkpoints: (u64::MAX - 20) + 10 = u64::MAX - 10, then u64::MAX - 10 + 10 = u64::MAX
        let checkpoint_blocks = [u64::MAX - 10, u64::MAX];

        let (provider, roots) = mock_with_blocks(&checkpoint_blocks);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0D);

        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: starting,
                l2_block_number: l2,
                intermediate_block_interval: interval,
                claimed_root: B256::ZERO,
                intermediate_roots: &roots,
            })
            .await;

        let validation = result.expect("should not overflow");
        assert!(validation.is_valid, "all roots should match");
    }

    #[rstest]
    #[case::equal(100, 100)]
    #[case::greater(200, 100)]
    #[tokio::test]
    async fn test_starting_block_gte_l2_block(
        #[case] starting_block_number: u64,
        #[case] l2_block_number: u64,
    ) {
        let (provider, _roots) = mock_with_blocks(&[]);
        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0C);

        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number,
                l2_block_number,
                intermediate_block_interval: 10,
                claimed_root: B256::ZERO,
                intermediate_roots: &[],
            })
            .await;

        assert!(
            matches!(result, Err(ValidatorError::InvalidBlockRange { .. })),
            "expected InvalidBlockRange for starting={starting_block_number}, l2={l2_block_number}, got: {result:?}"
        );
    }

    /// RPC error mid-stream during intermediate root validation propagates
    /// the error instead of silently succeeding.
    #[tokio::test]
    async fn test_validate_intermediate_roots_rpc_error() {
        // starting_block = 90, l2_block = 100, interval = 5
        // Checkpoint blocks: 95, 100
        // Block 95: fully configured (header + proof) -> succeeds
        // Block 100: header only, no proof -> RPC error
        let storage_hash = B256::repeat_byte(0xBB);
        let (header_95, account_95) = build_test_header_and_account(95, storage_hash);
        let root_95 =
            OutputRoot::from_parts(header_95.state_root, storage_hash, header_95.hash_slow())
                .hash();

        let (header_100, _) = build_test_header_and_account(100, storage_hash);
        let hash_100 = header_100.hash_slow();

        let mut provider = MockL2Provider::new();
        provider.insert_block(95, header_95, account_95);
        // Insert header for block 100 but omit the proof so get_proof fails.
        let rpc_header_100 = RpcHeader { hash: hash_100, inner: header_100, ..Default::default() };
        provider.headers.insert(100, rpc_header_100);

        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0E);

        let result = validator
            .validate_intermediate_roots(IntermediateValidationParams {
                game_address,
                starting_block_number: 90,
                l2_block_number: 100,
                intermediate_block_interval: 5,
                claimed_root: B256::ZERO,
                intermediate_roots: &[root_95, B256::ZERO],
            })
            .await;

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ValidatorError::Rpc(_)),
            "expected Rpc error variant"
        );
    }

    /// Header hash mismatch: RPC returns a header whose hash does not match the
    /// consensus header, and the validator rejects it.
    #[tokio::test]
    async fn test_header_hash_mismatch() {
        let storage_hash = B256::repeat_byte(0xBB);
        let (consensus_header, account) = build_test_header_and_account(100, storage_hash);
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

    /// Account proof verification failure: the header has one state root but the
    /// account proof is valid for a different state root.
    #[tokio::test]
    async fn test_account_proof_verification_failure() {
        let storage_hash = B256::repeat_byte(0xBB);
        let (_, account) = build_test_header_and_account(100, storage_hash);

        // Create a header with a different state root that won't match the proof
        let header_wrong_root = ConsensusHeader {
            number: 100,
            state_root: B256::repeat_byte(0xFF),
            ..Default::default()
        };

        let mut provider = MockL2Provider::new();
        // Insert the wrong-root header but the account proof built for a
        // different state root.
        let block_hash = header_wrong_root.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: header_wrong_root, ..Default::default() };
        provider.headers.insert(100, rpc_header);
        provider.proofs.insert(block_hash, account);

        let validator = OutputValidator::new(Arc::new(provider));
        let game_address = Address::repeat_byte(0x0F);

        let result = validator.validate_final_root(game_address, 100, B256::ZERO).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ValidatorError::AccountProofFailed { block_number: 100, .. }),
            "expected AccountProofFailed, got: {err:?}"
        );
    }
}
