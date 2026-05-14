//! Validation primitives.
//!
//! - [`AccountProofVerifier`] verifies an `eth_getProof` response against a
//!   state root using a Merkle Patricia Trie proof. Used as a guard against
//!   a compromised L2 RPC: even if the RPC lies about an account's storage
//!   hash, an invalid MPT proof exposes the lie.
//! - [`OutputValidator`] computes the expected output root at a given L2
//!   block by combining a trusted L2 header with a verified storage proof
//!   of the `L2ToL1MessagePasser` predeploy.

use std::sync::Arc;

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{
    Nibbles, TrieAccount,
    proof::{ProofVerificationError, verify_proof},
};
use async_trait::async_trait;
use base_common_consensus::Predeploys;
use base_proof_contracts::{AggregateVerifierClient, ContractError};
use base_proof_rpc::{L2Provider, RpcError};
use base_protocol::OutputRoot;
use futures::{StreamExt, TryStreamExt, stream};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::scanner::{GameInfo, GameSituation};

/// Errors returned by [`AccountProofVerifier::verify`].
#[derive(Debug, Eq, PartialEq, Error)]
pub enum AccountProofError {
    /// The RPC response is for a different account than the caller asked for.
    /// Indicates a buggy or malicious RPC endpoint.
    #[error("account proof address mismatch: expected {expected}, got {actual}")]
    AddressMismatch {
        /// The address the caller asked to verify.
        expected: Address,
        /// The address actually returned in the proof response.
        actual: Address,
    },

    /// The Merkle proof failed to verify against the supplied state root.
    /// Either the proof is malformed, the account fields don't match, or
    /// the state root is wrong.
    #[error("account proof verification failed: {0}")]
    VerificationFailed(#[from] ProofVerificationError),
}

/// Verifies `eth_getProof` responses against an L2 state root.
#[derive(Debug)]
pub struct AccountProofVerifier;

impl AccountProofVerifier {
    /// Verifies that `response` proves the account at `expected_address`
    /// exists in the trie rooted at `state_root` with the fields reported
    /// in the response.
    ///
    /// Returns [`AccountProofError::AddressMismatch`] if the response is
    /// for a different address than requested, or
    /// [`AccountProofError::VerificationFailed`] if the MPT proof does not
    /// validate against `state_root`.
    pub fn verify(
        response: &EIP1186AccountProofResponse,
        state_root: B256,
        expected_address: Address,
    ) -> Result<(), AccountProofError> {
        if response.address != expected_address {
            return Err(AccountProofError::AddressMismatch {
                expected: expected_address,
                actual: response.address,
            });
        }

        let key = Nibbles::unpack(keccak256(expected_address));

        let account = TrieAccount {
            nonce: response.nonce,
            balance: response.balance,
            storage_root: response.storage_hash,
            code_hash: response.code_hash,
        };

        let mut encoded = Vec::with_capacity(account.length());
        account.encode(&mut encoded);

        verify_proof(state_root, key, Some(encoded), &response.account_proof)?;
        Ok(())
    }
}

/// Errors returned by [`OutputValidator::compute_output_root`].
#[derive(Debug, Error)]
pub enum ValidatorError {
    /// The requested L2 block has not been produced yet.
    #[error("L2 block {block_number} is not yet available")]
    BlockNotAvailable {
        /// The block number that was requested.
        block_number: u64,
        /// The underlying [`RpcError`] (chained via [`std::error::Error::source`]).
        #[source]
        source: RpcError,
    },

    /// The hash returned by the RPC does not match the hash recomputed from
    /// the consensus header. Indicates a buggy or malicious RPC.
    #[error(
        "header hash mismatch at block {block_number}: rpc={rpc_hash}, computed={computed_hash}"
    )]
    HeaderHashMismatch {
        /// The block number where the mismatch was observed.
        block_number: u64,
        /// The hash returned by the RPC.
        rpc_hash: B256,
        /// The hash recomputed from the consensus header.
        computed_hash: B256,
    },

    /// The MPT account proof failed to verify against the header state root.
    #[error("account proof verification failed at block {block_number}")]
    AccountProofFailed {
        /// The block number where the proof verification failed.
        block_number: u64,
        /// The underlying [`AccountProofError`] (chained via [`std::error::Error::source`]).
        #[source]
        source: AccountProofError,
    },

    /// Underlying L2 RPC error not otherwise classified.
    #[error("L2 RPC error: {0}")]
    Rpc(#[from] RpcError),
}

/// Computes the expected output root at a given L2 block.
///
/// Implementations are expected to fetch a trusted L2 header and a verified
/// storage proof of the `L2ToL1MessagePasser` predeploy, then combine them
/// via [`OutputRoot::from_parts`].
#[async_trait]
pub trait OutputValidator: Send + Sync {
    /// Returns the output root that should appear at `block_number`
    /// according to the underlying L2 state.
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError>;
}

/// Concrete [`OutputValidator`] backed by a real L2 RPC client.
#[derive(Debug)]
pub struct L2OutputValidator<L2: L2Provider> {
    l2: Arc<L2>,
}

impl<L2: L2Provider> L2OutputValidator<L2> {
    /// Creates a new validator wrapping the provided L2 RPC client.
    pub const fn new(l2: Arc<L2>) -> Self {
        Self { l2 }
    }
}

#[async_trait]
impl<L2: L2Provider> OutputValidator for L2OutputValidator<L2> {
    async fn compute_output_root(&self, block_number: u64) -> Result<B256, ValidatorError> {
        let rpc_header =
            self.l2.header_by_number(Some(block_number)).await.map_err(|e| match e {
                e @ (RpcError::HeaderNotFound(_) | RpcError::BlockNotFound(_)) => {
                    ValidatorError::BlockNotAvailable { block_number, source: e }
                }
                e => ValidatorError::Rpc(e),
            })?;

        let rpc_hash = rpc_header.hash;
        let consensus = rpc_header.inner;
        let computed_hash = consensus.hash_slow();
        if rpc_hash != computed_hash {
            return Err(ValidatorError::HeaderHashMismatch {
                block_number,
                rpc_hash,
                computed_hash,
            });
        }

        let proof = self.l2.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, rpc_hash).await?;

        AccountProofVerifier::verify(
            &proof,
            consensus.state_root,
            Predeploys::L2_TO_L1_MESSAGE_PASSER,
        )
        .map_err(|source| ValidatorError::AccountProofFailed { block_number, source })?;

        Ok(OutputRoot::from_parts(consensus.state_root, proof.storage_hash, computed_hash).hash())
    }
}

/// A detected dispute-game violation.
///
/// Output of [`Violation::detect`], input of [`Violation::dispute_request`].
/// Carries everything needed to produce a proof and submit a
/// `DisputeAction` without re-fetching on-chain state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Dispute game proxy address.
    pub game_address: Address,
    /// L1 block hash captured at game creation (CWIA-immutable).
    pub l1_head: B256,
    /// L2 block interval between intermediate root checkpoints.
    pub intermediate_block_interval: u64,
    /// Intermediate root index where the violation was found.
    pub invalid_index: u64,
    /// Root we computed for `invalid_index` from our L2 RPC.
    /// Treated as the value to assert on-chain; only as good as our
    /// L2 RPC view (no independent consensus check).
    pub computed_root: B256,
    /// Predecessor root at `invalid_index - 1`, or the game's
    /// `startingOutputRoot` when `invalid_index == 0`.
    pub starting_root: B256,
    /// L2 block at the start of the disputed range.
    pub start_block: u64,
    /// L2 block at the end of the disputed range.
    pub end_block: u64,
    /// What kind of violation this is.
    pub situation: ViolationSituation,
}

impl Violation {
    /// Maximum number of intermediate-root checkpoints validated in
    /// parallel. Bounded fan-out so a backlog cannot saturate the L2
    /// RPC pool.
    const SCAN_CONCURRENCY: usize = 32;

    /// Re-fetches live on-chain state and compares the game's
    /// intermediate roots against what we compute from our L2 RPC.
    /// Returns `Some` when an actionable violation is found, `None`
    /// otherwise (game consistent with our view, terminal, or
    /// non-actionable state).
    pub async fn detect(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        verifier: &dyn AggregateVerifierClient,
    ) -> Result<Option<Self>, ValidationError> {
        // GameInfo.situation was captured at scan time; re-classify
        // against fresh prover state before acting.
        let (tee_prover, zk_prover, countered_index) = tokio::try_join!(
            verifier.tee_prover(game.address),
            verifier.zk_prover(game.address),
            verifier.countered_index(game.address),
        )?;

        let situation = match GameSituation::classify(tee_prover, zk_prover, countered_index) {
            Ok(s) => s,
            Err(_) => {
                warn!(
                    game = %game.address,
                    %tee_prover, %zk_prover, countered_index,
                    "unreachable on-chain prover tuple, skipping"
                );
                return Ok(None);
            }
        };
        debug!(game = %game.address, ?situation, "validating game");

        match situation {
            // Look for a TEE-side mismatch on any intermediate root. For
            // BothProven, both provers agree but L2 may not; if we find
            // a mismatch we nullify TEE first and let the next scan
            // catch any remaining ZK mismatch via the ZkOnly branch.
            GameSituation::TeeOnly | GameSituation::BothProven => {
                Self::scan_intermediate_roots(game, validator, ViolationSituation::TeeWrong).await
            }
            // Look for a ZK-side mismatch on any intermediate root.
            GameSituation::ZkOnly => {
                Self::scan_intermediate_roots(game, validator, ViolationSituation::ZkWrong).await
            }
            // Check the challenged checkpoint: if the on-chain TEE root
            // there matches what we compute, the ZK challenge looks
            // fraudulent and must be countered; otherwise the challenger
            // is legitimate from our vantage.
            GameSituation::UnderChallenge { challenged_index } => {
                Self::check_challenged_index(game, validator, challenged_index).await
            }
            // Nothing to validate: game is dead or already nullified.
            GameSituation::TeeNullifiedDuringChallenge | GameSituation::Terminal => Ok(None),
        }
    }

    /// Looks for any intermediate root that disagrees with what we
    /// compute. On a hit, returns a `Violation` for the lowest such
    /// index, tagged with `situation`. Returns `None` when every
    /// root matches our view.
    async fn scan_intermediate_roots(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        situation: ViolationSituation,
    ) -> Result<Option<Self>, ValidationError> {
        let interval = game.intermediate_block_interval;

        // Compute our view for every checkpoint in parallel and pair
        // each with its claimed root, then take the lowest-index
        // disagreement (deterministic regardless of completion order).
        let mismatch = stream::iter(game.intermediate_roots.iter().zip(0u64..))
            .map(|(&claimed, i)| async move {
                let block = Self::checkpoint_block(game.starting_l2_block, i + 1, interval);
                validator.compute_output_root(block).await.map(|computed| (i, claimed, computed))
            })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter(|(_, claimed, computed)| claimed != computed)
            .min_by_key(|(i, _, _)| *i);

        // Every root matches what we compute: nothing actionable
        // from our vantage.
        let Some((invalid_index, _, computed_root)) = mismatch else {
            return Ok(None);
        };

        let starting_root = Self::fetch_starting_root(game, validator, invalid_index).await?;
        let violation = Self::build(game, invalid_index, computed_root, starting_root, situation);
        info!(
            game = %game.address,
            invalid_index,
            ?situation,
            "violation detected"
        );
        Ok(Some(violation))
    }

    /// Compares only the contested checkpoint to what we compute (the
    /// ZK challenge targets exactly this index, others are out of
    /// scope). Returns a `FraudulentZkChallenge` violation when the
    /// on-chain TEE root at this index matches our view, `None` when
    /// it does not (the ZK challenger is legitimate from our vantage
    /// and will win on its own).
    async fn check_challenged_index(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        challenged_index: u64,
    ) -> Result<Option<Self>, ValidationError> {
        let interval = game.intermediate_block_interval;
        let idx = usize::try_from(challenged_index).expect("challenged_index fits in usize");
        let on_chain_tee_root = game.intermediate_roots[idx];

        // Our computed view at the contested checkpoint.
        let end_block =
            Self::checkpoint_block(game.starting_l2_block, challenged_index + 1, interval);
        let computed_root = validator.compute_output_root(end_block).await?;

        // On-chain TEE root diverges from what we compute: the ZK
        // challenger reached the same conclusion as us, let them
        // resolve in their favor on their own.
        if on_chain_tee_root != computed_root {
            return Ok(None);
        }

        // On-chain TEE matches our computed view: the ZK challenge
        // looks fraudulent from our vantage.
        let starting_root = Self::fetch_starting_root(game, validator, challenged_index).await?;
        let violation = Self::build(
            game,
            challenged_index,
            computed_root,
            starting_root,
            ViolationSituation::FraudulentZkChallenge { on_chain_tee_root },
        );
        info!(
            game = %game.address,
            challenged_index,
            "fraudulent ZK challenge detected"
        );
        Ok(Some(violation))
    }

    /// Returns the predecessor root for `invalid_index`. For index 0
    /// the predecessor is what we compute at the game's starting block.
    async fn fetch_starting_root(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        invalid_index: u64,
    ) -> Result<B256, ValidatorError> {
        if invalid_index == 0 {
            validator.compute_output_root(game.starting_l2_block).await
        } else {
            let prev = usize::try_from(invalid_index - 1).expect("invalid_index - 1 fits in usize");
            Ok(game.intermediate_roots[prev])
        }
    }

    /// Assembles a `Violation` from validated parts.
    const fn build(
        game: &GameInfo,
        invalid_index: u64,
        computed_root: B256,
        starting_root: B256,
        situation: ViolationSituation,
    ) -> Self {
        let interval = game.intermediate_block_interval;
        Self {
            game_address: game.address,
            l1_head: game.l1_head,
            intermediate_block_interval: interval,
            invalid_index,
            computed_root,
            starting_root,
            start_block: Self::checkpoint_block(game.starting_l2_block, invalid_index, interval),
            end_block: Self::checkpoint_block(game.starting_l2_block, invalid_index + 1, interval),
            situation,
        }
    }

    /// Returns the L2 block at the n-th checkpoint
    /// (`starting + n * interval`).
    const fn checkpoint_block(starting: u64, n: u64, interval: u64) -> u64 {
        starting + n * interval
    }
}

/// Errors returned by [`Violation::detect`].
#[derive(Debug, Error)]
pub enum ValidationError {
    /// L2 output root computation failed.
    #[error(transparent)]
    Output(#[from] ValidatorError),

    /// Reading on-chain game state from the aggregate verifier failed.
    #[error(transparent)]
    Contract(#[from] ContractError),
}

/// Kind of violation detected on a dispute game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationSituation {
    /// On-chain TEE root at `invalid_index` disagrees with our
    /// computed view. Resolved via the TEE-first dispute path,
    /// with ZK as fallback (see [`Violation::dispute_request`]).
    TeeWrong,
    /// On-chain ZK root at `invalid_index` disagrees with our
    /// computed view. Resolved by re-asserting our computed root
    /// via ZK (see [`Violation::dispute_request`]).
    ZkWrong,
    /// On-chain ZK challenge looks fraudulent from our vantage:
    /// the on-chain TEE root at `invalid_index` matches our
    /// computed view. Resolved by re-asserting the on-chain TEE
    /// root via ZK (see [`Violation::dispute_request`]).
    FraudulentZkChallenge {
        /// On-chain TEE root we will assert via the SNARK.
        on_chain_tee_root: B256,
    },
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address, b256};
    use alloy_rpc_types_eth::EIP1186AccountProofResponse;
    use alloy_trie::{HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};

    use super::*;

    /// Builds a minimal MPT containing a single account leaf and returns
    /// the resulting state root and an `eth_getProof`-shaped response.
    fn build_account_proof(
        address: Address,
        nonce: u64,
        balance: U256,
        storage_hash: B256,
        code_hash: B256,
    ) -> (B256, EIP1186AccountProofResponse) {
        let account = TrieAccount { nonce, balance, storage_root: storage_hash, code_hash };
        let mut encoded = Vec::with_capacity(account.length());
        account.encode(&mut encoded);

        let account_key = Nibbles::unpack(keccak256(address));
        let mut hb =
            HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![account_key]));
        hb.add_leaf(account_key, &encoded);
        let state_root = hb.root();
        let proof_nodes = hb.take_proof_nodes();
        let account_proof: Vec<Bytes> =
            proof_nodes.into_nodes_sorted().into_iter().map(|(_, v)| v).collect();

        let response = EIP1186AccountProofResponse {
            address,
            account_proof,
            balance,
            code_hash,
            nonce,
            storage_hash,
            storage_proof: vec![],
        };

        (state_root, response)
    }

    const TEST_ADDRESS: Address = address!("4200000000000000000000000000000000000016");
    const TEST_STORAGE_HASH: B256 =
        b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const TEST_CODE_HASH: B256 =
        b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");

    #[test]
    fn verify_valid_proof_succeeds() {
        let (state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn verify_rejects_address_mismatch() {
        let (state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        let other_address = address!("0000000000000000000000000000000000000bad");

        let result = AccountProofVerifier::verify(&response, state_root, other_address);

        assert_eq!(
            result,
            Err(AccountProofError::AddressMismatch {
                expected: other_address,
                actual: TEST_ADDRESS,
            })
        );
    }

    #[test]
    fn verify_rejects_wrong_state_root() {
        let (_state_root, response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        let bad_root = B256::repeat_byte(0xFF);

        let result = AccountProofVerifier::verify(&response, bad_root, TEST_ADDRESS);

        assert!(matches!(result, Err(AccountProofError::VerificationFailed(_))));
    }

    #[test]
    fn verify_rejects_tampered_account_field() {
        let (state_root, mut response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        response.balance = U256::from(1u64);

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        assert!(matches!(result, Err(AccountProofError::VerificationFailed(_))));
    }

    #[test]
    fn verify_rejects_empty_proof() {
        let (state_root, mut response) =
            build_account_proof(TEST_ADDRESS, 0, U256::ZERO, TEST_STORAGE_HASH, TEST_CODE_HASH);
        response.account_proof.clear();

        let result = AccountProofVerifier::verify(&response, state_root, TEST_ADDRESS);

        let err = result.expect_err("empty proof must fail");
        let AccountProofError::VerificationFailed(inner) = err else {
            panic!("expected VerificationFailed, got {err:?}");
        };
        let _ = inner;
    }

    mod output_validator {
        use std::sync::Arc;

        use alloy_primitives::B256;
        use alloy_rpc_types_eth::Header as RpcHeader;

        use super::*;
        use crate::test_utils::{
            MockL2Provider, build_account_at_block, build_message_passer_proof,
        };

        #[tokio::test]
        async fn compute_output_root_returns_expected_root() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, proof) = build_message_passer_proof(100, storage_hash);
            let state_root = header.state_root;
            let block_hash = header.hash_slow();
            let expected = OutputRoot::from_parts(state_root, storage_hash, block_hash).hash();

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header, proof);

            let validator = L2OutputValidator::new(Arc::new(provider));
            let got = validator.compute_output_root(100).await.expect("must succeed");

            assert_eq!(got, expected);
        }

        #[tokio::test]
        async fn compute_output_root_returns_block_not_available() {
            let mut provider = MockL2Provider::new();
            provider.error_blocks.push(999);
            let validator = L2OutputValidator::new(Arc::new(provider));

            let err = validator.compute_output_root(999).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::BlockNotAvailable { block_number: 999, .. }),
                "expected BlockNotAvailable, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_returns_block_not_available_when_header_missing() {
            let provider = MockL2Provider::new();
            let validator = L2OutputValidator::new(Arc::new(provider));

            let err = validator.compute_output_root(42).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::BlockNotAvailable { block_number: 42, .. }),
                "expected BlockNotAvailable, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_rejects_header_hash_mismatch() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, proof) = build_message_passer_proof(100, storage_hash);
            let computed_hash = header.hash_slow();
            let tampered_hash = B256::repeat_byte(0xEE);

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header.clone(), proof);
            provider.headers.insert(
                100,
                RpcHeader { hash: tampered_hash, inner: header, ..Default::default() },
            );

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            match err {
                ValidatorError::HeaderHashMismatch { block_number, rpc_hash, computed_hash: c } => {
                    assert_eq!(block_number, 100);
                    assert_eq!(rpc_hash, tampered_hash);
                    assert_eq!(c, computed_hash);
                }
                other => panic!("expected HeaderHashMismatch, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn compute_output_root_rejects_proof_for_wrong_address() {
            let storage_hash = B256::repeat_byte(0xBB);
            let other_address = address!("00000000000000000000000000000000000000ff");
            let (header, proof) =
                build_account_at_block(100, other_address, 0, U256::ZERO, storage_hash, B256::ZERO);

            let mut provider = MockL2Provider::new();
            provider.insert_block(100, header, proof);

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            assert!(
                matches!(err, ValidatorError::AccountProofFailed { block_number: 100, .. }),
                "expected AccountProofFailed, got {err:?}"
            );
        }

        #[tokio::test]
        async fn compute_output_root_propagates_other_rpc_errors() {
            let storage_hash = B256::repeat_byte(0xBB);
            let (header, _proof) = build_message_passer_proof(100, storage_hash);
            let mut provider = MockL2Provider::new();
            let block_hash = header.hash_slow();
            provider
                .headers
                .insert(100, RpcHeader { hash: block_hash, inner: header, ..Default::default() });
            // No proof inserted under block_hash, so get_proof returns ProofNotFound.

            let validator = L2OutputValidator::new(Arc::new(provider));
            let err = validator.compute_output_root(100).await.expect_err("must fail");

            assert!(matches!(err, ValidatorError::Rpc(_)), "expected Rpc error, got {err:?}");
        }
    }

    mod detect {
        use super::*;
        use crate::test_utils::{MockAggregateVerifier, MockGameState, MockOutputValidator, addr};

        const STARTING_BLOCK: u64 = 100;
        const INTERVAL: u64 = 5;

        fn root(i: u8) -> B256 {
            B256::repeat_byte(i)
        }

        fn build_game(intermediate_roots: Vec<B256>, situation: GameSituation) -> GameInfo {
            let len = intermediate_roots.len() as u64;
            GameInfo {
                address: addr(42),
                factory_index: 0,
                root_claim: root(99),
                l1_head: root(1),
                l2_block_number: STARTING_BLOCK + INTERVAL * len,
                starting_l2_block: STARTING_BLOCK,
                intermediate_roots: intermediate_roots.into_boxed_slice(),
                intermediate_block_interval: INTERVAL,
                situation,
            }
        }

        fn build_state(situation: GameSituation) -> MockGameState {
            let (tee, zk, c) = match situation {
                GameSituation::TeeOnly => (addr(1), Address::ZERO, 0),
                GameSituation::ZkOnly => (Address::ZERO, addr(2), 0),
                GameSituation::BothProven => (addr(1), addr(2), 0),
                GameSituation::UnderChallenge { challenged_index } => {
                    (addr(1), addr(2), challenged_index + 1)
                }
                GameSituation::TeeNullifiedDuringChallenge => (Address::ZERO, addr(2), 1),
                GameSituation::Terminal => (Address::ZERO, Address::ZERO, 0),
            };
            MockGameState::in_progress(tee, zk, c)
        }

        /// Builds a `(GameInfo, validator, verifier)` triple where the
        /// validator returns `expected_at[i]` for block `STARTING_BLOCK +
        /// (i+1)*INTERVAL` and the verifier reflects `situation`.
        fn fixture(
            intermediate_roots: Vec<B256>,
            expected_at: &[B256],
            situation: GameSituation,
        ) -> (GameInfo, MockOutputValidator, MockAggregateVerifier) {
            let game = build_game(intermediate_roots, situation);
            let validator = MockOutputValidator::new();
            for (i, r) in expected_at.iter().enumerate() {
                let block = STARTING_BLOCK + (i as u64 + 1) * INTERVAL;
                validator.set(block, *r);
            }
            let verifier = MockAggregateVerifier::new();
            verifier.set_game(game.address, build_state(situation));
            (game, validator, verifier)
        }

        #[tokio::test]
        async fn tee_only_all_correct_returns_none() {
            let r0 = root(10);
            let r1 = root(11);
            let r2 = root(12);
            let (game, v, c) = fixture(vec![r0, r1, r2], &[r0, r1, r2], GameSituation::TeeOnly);
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn tee_only_index_2_wrong_returns_tee_wrong() {
            let r0 = root(10);
            let r1 = root(11);
            let r2 = root(12);
            let expected_r2 = root(99);
            let (game, v, c) =
                fixture(vec![r0, r1, r2], &[r0, r1, expected_r2], GameSituation::TeeOnly);

            let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
            assert_eq!(violation.situation, ViolationSituation::TeeWrong);
            assert_eq!(violation.invalid_index, 2);
            assert_eq!(violation.computed_root, expected_r2);
            assert_eq!(violation.starting_root, r1);
            assert_eq!(violation.start_block, STARTING_BLOCK + 2 * INTERVAL);
            assert_eq!(violation.end_block, STARTING_BLOCK + 3 * INTERVAL);
            assert_eq!(violation.intermediate_block_interval, INTERVAL);
            assert_eq!(violation.l1_head, root(1));
            assert_eq!(violation.game_address, addr(42));
        }

        #[tokio::test]
        async fn zk_only_index_0_wrong_uses_computed_starting_root() {
            let r0 = root(10);
            let expected_r0 = root(99);
            let starting_expected = root(50);
            let (game, v, c) = fixture(vec![r0], &[expected_r0], GameSituation::ZkOnly);
            // For invalid_index == 0, detect fetches the validator's
            // output at starting_l2_block as the predecessor root.
            v.set(STARTING_BLOCK, starting_expected);

            let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
            assert_eq!(violation.situation, ViolationSituation::ZkWrong);
            assert_eq!(violation.invalid_index, 0);
            assert_eq!(violation.computed_root, expected_r0);
            assert_eq!(violation.starting_root, starting_expected);
            assert_eq!(violation.start_block, STARTING_BLOCK);
            assert_eq!(violation.end_block, STARTING_BLOCK + INTERVAL);
        }

        #[tokio::test]
        async fn zk_only_all_correct_returns_none() {
            let r0 = root(10);
            let r1 = root(11);
            let (game, v, c) = fixture(vec![r0, r1], &[r0, r1], GameSituation::ZkOnly);
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn both_proven_index_1_wrong_returns_tee_wrong() {
            let r0 = root(10);
            let r1 = root(11);
            let expected_r1 = root(99);
            let (game, v, c) = fixture(vec![r0, r1], &[r0, expected_r1], GameSituation::BothProven);

            let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
            // BothProven nullifies TEE first; ZK gets caught on a later scan.
            assert_eq!(violation.situation, ViolationSituation::TeeWrong);
            assert_eq!(violation.invalid_index, 1);
            assert_eq!(violation.computed_root, expected_r1);
        }

        #[tokio::test]
        async fn both_proven_all_correct_returns_none() {
            let r0 = root(10);
            let (game, v, c) = fixture(vec![r0], &[r0], GameSituation::BothProven);
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn under_challenge_with_correct_tee_returns_fraudulent() {
            let r0 = root(10);
            let r1 = root(11);
            let challenged_index = 1;
            let (game, v, c) = fixture(
                vec![r0, r1],
                &[r0, r1],
                GameSituation::UnderChallenge { challenged_index },
            );

            let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
            match violation.situation {
                ViolationSituation::FraudulentZkChallenge { on_chain_tee_root } => {
                    assert_eq!(on_chain_tee_root, r1);
                }
                other => panic!("expected FraudulentZkChallenge, got {other:?}"),
            }
            assert_eq!(violation.invalid_index, 1);
            assert_eq!(violation.computed_root, r1);
            assert_eq!(violation.starting_root, r0);
        }

        #[tokio::test]
        async fn under_challenge_with_diverging_tee_returns_none() {
            let r0 = root(10);
            let on_chain_r1 = root(11);
            let expected_r1 = root(99);
            let challenged_index = 1;
            let (game, v, c) = fixture(
                vec![r0, on_chain_r1],
                &[r0, expected_r1],
                GameSituation::UnderChallenge { challenged_index },
            );
            // On-chain TEE diverges from our view, so the ZK challenger
            // reached the same conclusion: skip.
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn terminal_returns_none() {
            let game = build_game(vec![root(10)], GameSituation::Terminal);
            let v = MockOutputValidator::new();
            let c = MockAggregateVerifier::new();
            c.set_game(game.address, build_state(GameSituation::Terminal));
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn tee_nullified_during_challenge_returns_none() {
            let game = build_game(vec![root(10)], GameSituation::TeeNullifiedDuringChallenge);
            let v = MockOutputValidator::new();
            let c = MockAggregateVerifier::new();
            c.set_game(game.address, build_state(GameSituation::TeeNullifiedDuringChallenge));
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }

        #[tokio::test]
        async fn unreachable_state_returns_none() {
            // tee=0, zk=0, countered>0 is unreachable per classify().
            let game = build_game(vec![root(10)], GameSituation::TeeOnly);
            let v = MockOutputValidator::new();
            let c = MockAggregateVerifier::new();
            c.set_game(game.address, MockGameState::in_progress(Address::ZERO, Address::ZERO, 7));
            assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
        }
    }
}
