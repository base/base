//! Output root validator for dispute game candidates.
//!
//! Given a [`CandidateGame`] produced by the [`GameScanner`](crate::GameScanner), the
//! [`OutputValidator`] fetches the corresponding L2 block header and
//! `L2ToL1MessagePasser` storage root via the L2 RPC, computes the expected output root
//! using the OP Stack v0 formula, and compares it against the game's on-chain `rootClaim`.
//!
//! For intermediate roots it reads `INTERMEDIATE_BLOCK_INTERVAL` from the
//! `AggregateVerifier`, iterates checkpoint blocks, and compares them against the
//! intermediate roots stored in the game's `extraData`.

use std::sync::Arc;
use std::time::Instant;

use alloy_primitives::{Address, B256};
use base_enclave::AccountResult;
use base_proof_contracts::{AggregateVerifierClient, ContractError, decode_extra_data};
use base_proof_rpc::{L2Provider, RpcError};
use base_protocol::{OutputRoot, Predeploys};
use thiserror::Error;
use tracing::{info, warn};

use crate::ChallengerMetrics;

/// Errors that can occur during output root validation.
#[derive(Debug, Error)]
pub enum ValidatorError {
    /// An L2 RPC call failed.
    #[error("L2 RPC error: {0}")]
    Rpc(#[from] RpcError),

    /// A contract read failed.
    #[error("contract error: {0}")]
    Contract(#[from] ContractError),

    /// The requested L2 block has not been produced yet.
    #[error("L2 block not found: {block_number}")]
    BlockNotFound {
        /// The block number that was not found.
        block_number: u64,
    },

    /// The game's `extraData` could not be decoded.
    #[error("invalid extra data: {0}")]
    InvalidExtraData(String),
}

/// Result of validating a dispute game's output root.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// The game proxy address.
    pub game_address: Address,
    /// The game's factory index.
    pub factory_index: u64,
    /// The expected output root computed from L2 state.
    pub expected_root: B256,
    /// The output root claimed on-chain.
    pub claimed_root: B256,
    /// Whether the final output root is valid.
    pub is_valid: bool,
    /// Index of the first failing intermediate root, if any.
    pub invalid_intermediate_index: Option<u64>,
}

/// Result of validating a single intermediate checkpoint root.
#[derive(Debug, Clone)]
pub struct IntermediateValidationResult {
    /// The L2 block number of the checkpoint.
    pub block_number: u64,
    /// The expected output root at this checkpoint.
    pub expected_root: B256,
    /// The claimed output root at this checkpoint.
    pub claimed_root: B256,
    /// Whether this checkpoint root is valid.
    pub is_valid: bool,
}

/// Validates output roots for candidate dispute games.
///
/// The validator is generic over the L2 provider so that tests can supply a mock
/// implementation without network access.
pub struct OutputValidator<L2: L2Provider> {
    l2_client: Arc<L2>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    impl_address: Address,
}

impl<L2: L2Provider> std::fmt::Debug for OutputValidator<L2> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputValidator")
            .field("impl_address", &self.impl_address)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider> OutputValidator<L2> {
    /// Creates a new output validator.
    ///
    /// `impl_address` is the `AggregateVerifier` implementation contract used
    /// to read `INTERMEDIATE_BLOCK_INTERVAL`.
    pub fn new(
        l2_client: Arc<L2>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        impl_address: Address,
    ) -> Self {
        Self { l2_client, verifier_client, impl_address }
    }

    /// Validates the final output root of a candidate game.
    ///
    /// Fetches the L2 block header and `L2ToL1MessagePasser` storage root, computes the
    /// expected output root, and compares it against the game's `rootClaim`.
    pub async fn validate_final_root(
        &self,
        game: &crate::CandidateGame,
    ) -> Result<ValidationResult, ValidatorError> {
        let start = Instant::now();
        let game_address = game.factory.proxy;
        let block_number = game.info.l2_block_number;

        let expected_root = self.compute_output_root(block_number).await?;
        let claimed_root = game.info.root_claim;
        let is_valid = expected_root == claimed_root;

        if is_valid {
            info!(
                game = %game_address,
                index = game.index,
                "final output root validated"
            );
        } else {
            warn!(
                game = %game_address,
                index = game.index,
                expected = %expected_root,
                claimed = %claimed_root,
                "final root mismatch detected"
            );
            metrics::counter!(ChallengerMetrics::GAMES_INVALID_TOTAL).increment(1);
        }

        let elapsed = start.elapsed();
        metrics::histogram!(ChallengerMetrics::VALIDATION_LATENCY_SECONDS)
            .record(elapsed.as_secs_f64());

        Ok(ValidationResult {
            game_address,
            factory_index: game.index,
            expected_root,
            claimed_root,
            is_valid,
            invalid_intermediate_index: None,
        })
    }

    /// Validates the intermediate checkpoint roots of a candidate game.
    ///
    /// Reads `INTERMEDIATE_BLOCK_INTERVAL` and the game's `extraData`, then checks
    /// each checkpoint block from `starting_block_number + interval` up to
    /// `l2_block_number`.
    pub async fn validate_intermediate_roots(
        &self,
        game: &crate::CandidateGame,
    ) -> Result<Vec<IntermediateValidationResult>, ValidatorError> {
        let game_address = game.factory.proxy;

        let interval = self
            .verifier_client
            .read_intermediate_block_interval(self.impl_address)
            .await?;

        let extra_data_bytes = self.verifier_client.extra_data(game_address).await?;

        let (_l2_block, _parent_idx, intermediate_roots) =
            decode_extra_data(&extra_data_bytes).ok_or_else(|| {
                ValidatorError::InvalidExtraData("failed to decode game extraData".into())
            })?;

        let mut results = Vec::with_capacity(intermediate_roots.len());

        for (i, claimed_root) in intermediate_roots.iter().enumerate() {
            let checkpoint_block =
                game.starting_block_number + (i as u64 + 1) * interval;

            // Stop if the checkpoint is beyond the game's L2 block number.
            if checkpoint_block > game.info.l2_block_number {
                break;
            }

            let expected_root = self.compute_output_root(checkpoint_block).await?;
            let is_valid = expected_root == *claimed_root;

            if !is_valid {
                warn!(
                    game = %game_address,
                    index = game.index,
                    checkpoint_index = i,
                    block_number = checkpoint_block,
                    expected = %expected_root,
                    claimed = %claimed_root,
                    "intermediate root mismatch detected"
                );
            }

            results.push(IntermediateValidationResult {
                block_number: checkpoint_block,
                expected_root,
                claimed_root: *claimed_root,
                is_valid,
            });
        }

        Ok(results)
    }

    /// Computes the expected output root at the given L2 block number.
    ///
    /// Fetches the block header and `L2ToL1MessagePasser` storage proof, then constructs
    /// an [`OutputRoot`] using the OP Stack v0 formula.
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError> {
        let header = self
            .l2_client
            .header_by_number(Some(block_number))
            .await
            .map_err(|e| match &e {
                RpcError::HeaderNotFound(_) | RpcError::BlockNotFound(_) => {
                    ValidatorError::BlockNotFound { block_number }
                }
                _ => ValidatorError::Rpc(e),
            })?;

        let proof: AccountResult = self
            .l2_client
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, header.hash)
            .await
            .map_err(ValidatorError::Rpc)?;

        let output_root =
            OutputRoot::from_parts(header.inner.state_root, proof.storage_hash, header.hash);

        Ok(output_root.hash())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_consensus::Header as ConsensusHeader;
    use alloy_primitives::{Bytes, U256};
    use alloy_rpc_types_eth::Header;
    use async_trait::async_trait;
    use base_proof_contracts::{GameAtIndex, GameInfo, encode_extra_data};
    use base_proof_rpc::{OpBlock, RpcResult};

    use super::*;
    use crate::test_utils::{MockAggregateVerifier, MockGameState};

    // ── Mock L2 provider ───────────────────────────────────────────────

    /// Mock L2 provider that returns pre-configured headers and proofs.
    #[derive(Debug)]
    struct MockL2Provider {
        /// Block headers keyed by block number.
        headers: HashMap<u64, Header>,
        /// Account proofs keyed by block hash.
        proofs: HashMap<B256, AccountResult>,
    }

    #[async_trait]
    impl L2Provider for MockL2Provider {
        async fn chain_config(&self) -> RpcResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }

        async fn get_proof(&self, _address: Address, block_hash: B256) -> RpcResult<AccountResult> {
            self.proofs
                .get(&block_hash)
                .cloned()
                .ok_or_else(|| RpcError::ProofNotFound(format!("block hash {block_hash}")))
        }

        async fn header_by_number(&self, number: Option<u64>) -> RpcResult<Header> {
            let num = number.unwrap_or(0);
            self.headers
                .get(&num)
                .cloned()
                .ok_or_else(|| RpcError::HeaderNotFound(format!("block {num}")))
        }

        async fn block_by_number(&self, _number: Option<u64>) -> RpcResult<OpBlock> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }

        async fn block_by_hash(&self, _hash: B256) -> RpcResult<OpBlock> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────

    /// Builds an RPC header with the given state root and a deterministic block hash.
    fn make_header(block_number: u64, state_root: B256) -> Header {
        let inner = ConsensusHeader { state_root, number: block_number, ..Default::default() };
        let hash = inner.hash_slow();
        Header { hash, inner, total_difficulty: None, size: None }
    }

    /// Builds a minimal `AccountResult` with the given storage hash.
    fn make_proof(storage_hash: B256) -> AccountResult {
        AccountResult {
            address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
            account_proof: vec![],
            balance: U256::ZERO,
            code_hash: B256::ZERO,
            nonce: U256::ZERO,
            storage_hash,
            storage_proof: vec![],
        }
    }

    /// Creates a test address from a `u64` value.
    fn addr(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&index.to_be_bytes());
        Address::from(bytes)
    }

    /// Computes the expected output root for the given header and storage hash.
    fn expected_root(header: &Header, storage_hash: B256) -> B256 {
        OutputRoot::from_parts(header.inner.state_root, storage_hash, header.hash).hash()
    }

    /// Creates a `CandidateGame` from parts.
    fn candidate_game(
        index: u64,
        proxy: Address,
        root_claim: B256,
        l2_block_number: u64,
        starting_block_number: u64,
    ) -> crate::CandidateGame {
        crate::CandidateGame {
            index,
            factory: GameAtIndex { game_type: 1, timestamp: 1_000_000, proxy },
            info: GameInfo { root_claim, l2_block_number, parent_index: 0 },
            starting_block_number,
        }
    }

    /// Builds a mock L2 provider and verifier for a simple single-block game.
    fn setup_single_block(
        block_number: u64,
        state_root: B256,
        storage_hash: B256,
        proxy: Address,
        root_claim: B256,
    ) -> (MockL2Provider, MockAggregateVerifier) {
        let header = make_header(block_number, state_root);
        let proof = make_proof(storage_hash);

        let mut headers = HashMap::new();
        headers.insert(block_number, header);

        let mut proofs = HashMap::new();
        let block_hash = headers[&block_number].hash;
        proofs.insert(block_hash, proof);

        let l2 = MockL2Provider { headers, proofs };

        let mut games = HashMap::new();
        games.insert(
            proxy,
            MockGameState {
                status: 0,
                zk_prover: Address::ZERO,
                game_info: GameInfo { root_claim, l2_block_number: block_number, parent_index: 0 },
                starting_block_number: block_number.saturating_sub(10),
                extra_data: Bytes::new(),
            },
        );
        let verifier = MockAggregateVerifier { games };

        (l2, verifier)
    }

    // ── Tests ──────────────────────────────────────────────────────────

    /// Scenario 1: Final root matches the L2 state — game is valid.
    #[tokio::test]
    async fn test_valid_final_root() {
        let state_root = B256::repeat_byte(0x11);
        let storage_hash = B256::repeat_byte(0x22);
        let proxy = addr(1);

        let header = make_header(100, state_root);
        let correct_root = expected_root(&header, storage_hash);

        let (l2, verifier) = setup_single_block(100, state_root, storage_hash, proxy, correct_root);

        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(0, proxy, correct_root, 100, 90);
        let result = validator.validate_final_root(&game).await.unwrap();

        assert!(result.is_valid);
        assert_eq!(result.expected_root, correct_root);
        assert_eq!(result.claimed_root, correct_root);
        assert!(result.invalid_intermediate_index.is_none());
    }

    /// Scenario 2: Final root does NOT match the L2 state — game is invalid.
    #[tokio::test]
    async fn test_invalid_final_root() {
        let state_root = B256::repeat_byte(0x11);
        let storage_hash = B256::repeat_byte(0x22);
        let proxy = addr(2);

        let wrong_claim = B256::repeat_byte(0xFF);

        let (l2, verifier) = setup_single_block(200, state_root, storage_hash, proxy, wrong_claim);

        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(1, proxy, wrong_claim, 200, 190);
        let result = validator.validate_final_root(&game).await.unwrap();

        assert!(!result.is_valid);
        assert_ne!(result.expected_root, result.claimed_root);
    }

    /// Scenario 3: All intermediate checkpoint roots are valid.
    #[tokio::test]
    async fn test_valid_intermediate_roots() {
        let proxy = addr(3);
        let starting_block = 100u64;
        let l2_block = 120u64;
        // INTERMEDIATE_BLOCK_INTERVAL = 5 (from MockAggregateVerifier)
        // Checkpoints at: 105, 110, 115, 120 — 4 checkpoints

        let mut headers = HashMap::new();
        let mut proofs = HashMap::new();
        let mut intermediate_roots = Vec::new();

        for checkpoint in [105, 110, 115, 120] {
            let sr = B256::repeat_byte(checkpoint as u8);
            let sh = B256::repeat_byte((checkpoint + 1) as u8);
            let header = make_header(checkpoint, sr);
            let root = expected_root(&header, sh);

            proofs.insert(header.hash, make_proof(sh));
            headers.insert(checkpoint, header);
            intermediate_roots.push(root);
        }

        let extra_data = encode_extra_data(l2_block, 0, &intermediate_roots);

        let mut games = HashMap::new();
        games.insert(
            proxy,
            MockGameState {
                status: 0,
                zk_prover: Address::ZERO,
                game_info: GameInfo {
                    root_claim: B256::ZERO,
                    l2_block_number: l2_block,
                    parent_index: 0,
                },
                starting_block_number: starting_block,
                extra_data,
            },
        );

        let l2 = MockL2Provider { headers, proofs };
        let verifier = MockAggregateVerifier { games };
        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(2, proxy, B256::ZERO, l2_block, starting_block);
        let results = validator.validate_intermediate_roots(&game).await.unwrap();

        assert_eq!(results.len(), 4);
        for r in &results {
            assert!(r.is_valid, "checkpoint at block {} should be valid", r.block_number);
        }
    }

    /// Scenario 4: One intermediate root is wrong — reports correct failing index.
    #[tokio::test]
    async fn test_invalid_intermediate_root_reports_index() {
        let proxy = addr(4);
        let starting_block = 100u64;
        let l2_block = 115u64;
        // Checkpoints at: 105, 110, 115

        let mut headers = HashMap::new();
        let mut proofs = HashMap::new();
        let mut intermediate_roots = Vec::new();

        for checkpoint in [105, 110, 115] {
            let sr = B256::repeat_byte(checkpoint as u8);
            let sh = B256::repeat_byte((checkpoint + 1) as u8);
            let header = make_header(checkpoint, sr);
            let root = expected_root(&header, sh);

            proofs.insert(header.hash, make_proof(sh));
            headers.insert(checkpoint, header);
            intermediate_roots.push(root);
        }

        // Corrupt the second intermediate root (index 1, checkpoint block 110).
        intermediate_roots[1] = B256::repeat_byte(0xFF);

        let extra_data = encode_extra_data(l2_block, 0, &intermediate_roots);

        let mut games = HashMap::new();
        games.insert(
            proxy,
            MockGameState {
                status: 0,
                zk_prover: Address::ZERO,
                game_info: GameInfo {
                    root_claim: B256::ZERO,
                    l2_block_number: l2_block,
                    parent_index: 0,
                },
                starting_block_number: starting_block,
                extra_data,
            },
        );

        let l2 = MockL2Provider { headers, proofs };
        let verifier = MockAggregateVerifier { games };
        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(3, proxy, B256::ZERO, l2_block, starting_block);
        let results = validator.validate_intermediate_roots(&game).await.unwrap();

        assert_eq!(results.len(), 3);
        assert!(results[0].is_valid, "index 0 should be valid");
        assert!(!results[1].is_valid, "index 1 should be invalid");
        assert_eq!(results[1].block_number, 110);
        assert!(results[2].is_valid, "index 2 should be valid");
    }

    /// Scenario 5: Final root is valid but one intermediate root is invalid.
    #[tokio::test]
    async fn test_final_valid_intermediate_invalid() {
        let proxy = addr(5);
        let starting_block = 100u64;
        let l2_block = 112u64;
        // INTERMEDIATE_BLOCK_INTERVAL = 5, checkpoints at: 105, 110 (both < 112)

        let state_root = B256::repeat_byte(0xAA);
        let storage_hash = B256::repeat_byte(0xBB);
        let final_header = make_header(l2_block, state_root);
        let correct_final_root = expected_root(&final_header, storage_hash);

        let mut headers = HashMap::new();
        let mut proofs = HashMap::new();

        // Final block
        proofs.insert(final_header.hash, make_proof(storage_hash));
        headers.insert(l2_block, final_header);

        // Checkpoint blocks (105, 110 — neither overlaps l2_block=112)
        let mut intermediate_roots = Vec::new();
        for checkpoint in [105, 110] {
            let sr = B256::repeat_byte(checkpoint as u8);
            let sh = B256::repeat_byte((checkpoint + 1) as u8);
            let header = make_header(checkpoint, sr);
            let root = expected_root(&header, sh);

            proofs.insert(header.hash, make_proof(sh));
            headers.insert(checkpoint, header);
            intermediate_roots.push(root);
        }

        // Corrupt first intermediate root (index 0, checkpoint block 105)
        intermediate_roots[0] = B256::repeat_byte(0xFF);

        let extra_data = encode_extra_data(l2_block, 0, &intermediate_roots);

        let mut games = HashMap::new();
        games.insert(
            proxy,
            MockGameState {
                status: 0,
                zk_prover: Address::ZERO,
                game_info: GameInfo {
                    root_claim: correct_final_root,
                    l2_block_number: l2_block,
                    parent_index: 0,
                },
                starting_block_number: starting_block,
                extra_data,
            },
        );

        let l2 = MockL2Provider { headers, proofs };
        let verifier = MockAggregateVerifier { games };
        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(4, proxy, correct_final_root, l2_block, starting_block);

        // Final root should be valid
        let final_result = validator.validate_final_root(&game).await.unwrap();
        assert!(final_result.is_valid);

        // But intermediate root at index 0 should fail
        let intermediate_results =
            validator.validate_intermediate_roots(&game).await.unwrap();
        assert!(!intermediate_results[0].is_valid);
        assert!(intermediate_results[1].is_valid);
    }

    /// Scenario 6: L2 block not yet produced — returns `BlockNotFound` error.
    #[tokio::test]
    async fn test_missing_l2_block() {
        let proxy = addr(6);
        let block_number = 999u64;

        // Empty L2 provider — no headers or proofs at all.
        let l2 = MockL2Provider { headers: HashMap::new(), proofs: HashMap::new() };

        let mut games = HashMap::new();
        games.insert(
            proxy,
            MockGameState {
                status: 0,
                zk_prover: Address::ZERO,
                game_info: GameInfo {
                    root_claim: B256::ZERO,
                    l2_block_number: block_number,
                    parent_index: 0,
                },
                starting_block_number: block_number.saturating_sub(10),
                extra_data: Bytes::new(),
            },
        );

        let verifier = MockAggregateVerifier { games };
        let validator =
            OutputValidator::new(Arc::new(l2), Arc::new(verifier), Address::ZERO);

        let game = candidate_game(5, proxy, B256::ZERO, block_number, block_number - 10);
        let err = validator.validate_final_root(&game).await.unwrap_err();

        match err {
            ValidatorError::BlockNotFound { block_number: bn } => {
                assert_eq!(bn, block_number);
            }
            other => panic!("expected BlockNotFound, got: {other}"),
        }
    }
}
