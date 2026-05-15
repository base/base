//! Violation detection.
//!
//! [`Violation::detect`] re-fetches live on-chain prover state and
//! compares the game's intermediate roots against what we compute
//! from our L2 RPC. Returns `Some(Violation)` when a mismatch is
//! found, `None` when the game does not currently need action from
//! us.

use alloy_primitives::{Address, B256};
use base_proof_contracts::{AggregateVerifierClient, ContractError, GameStatus};
use futures::{StreamExt, TryStreamExt, stream};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::{
    game_discovery::{GameInfo, ProvingState},
    output_validator::{OutputRootError, OutputValidator},
};

/// A detected dispute-game violation.
///
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
    pub kind: ViolationKind,
}

impl Violation {
    /// Maximum number of intermediate-root checkpoints validated in
    /// parallel. Bounded fan-out so a backlog cannot saturate the L2
    /// RPC pool.
    const SCAN_CONCURRENCY: usize = 32;

    /// Re-fetches live on-chain state and compares the game's
    /// intermediate roots against what we compute from our L2 RPC.
    /// Returns `Some` when a violation is found, `None` otherwise
    /// (game resolved, every checkpoint matches, or no proof is
    /// currently attached).
    pub async fn detect(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        verifier: &dyn AggregateVerifierClient,
    ) -> Result<Option<Self>, ValidationError> {
        // Re-read live state (status + prover tuple) in parallel.
        // GameInfo carries a snapshot from the scanner that may be
        // stale by the time the worker acts.
        let (status, tee_prover, zk_prover, countered_index) = tokio::try_join!(
            verifier.status(game.address),
            verifier.tee_prover(game.address),
            verifier.zk_prover(game.address),
            verifier.countered_index(game.address),
        )?;

        // Game has resolved on-chain: any dispute we'd produce now
        // would be rejected at submission.
        if status != GameStatus::InProgress {
            debug!(game = %game.address, %status, "game no longer in progress");
            return Ok(None);
        }

        let proving_state = match ProvingState::classify(tee_prover, zk_prover, countered_index) {
            Ok(s) => s,
            Err(_) => {
                warn!(
                    game = %game.address,
                    %tee_prover, %zk_prover, countered_index,
                    "unreachable on-chain prover tuple"
                );
                return Ok(None);
            }
        };
        debug!(game = %game.address, ?proving_state, "validating game");

        match proving_state {
            // A TEE proof asserts the intermediate roots and the
            // game is not under challenge. Verify every attested
            // root.
            ProvingState::TeeOnly | ProvingState::BothProven => {
                Self::scan_intermediate_roots(game, validator, ViolationKind::TeeWrong).await
            }
            // A ZK proof asserts the intermediate roots and the
            // game is not under challenge. Verify every attested
            // root.
            ProvingState::ZkOnly => {
                Self::scan_intermediate_roots(game, validator, ViolationKind::ZkWrong).await
            }
            // A ZK challenge contests the root at
            // `challenged_index`. Verify that single root against
            // our view.
            ProvingState::UnderChallenge { challenged_index }
            | ProvingState::TeeNullifiedDuringChallenge { challenged_index } => {
                Self::check_challenged_index(game, validator, challenged_index).await
            }
            // No proof attached and no challenge to counter.
            // Nothing to verify.
            ProvingState::AllNullified => Ok(None),
        }
    }

    /// Looks for any intermediate root that disagrees with what we
    /// compute. On a hit, returns a `Violation` for the lowest such
    /// index, tagged with `kind`. Returns `None` when every
    /// root matches our view.
    async fn scan_intermediate_roots(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        kind: ViolationKind,
    ) -> Result<Option<Self>, ValidationError> {
        let interval = game.intermediate_block_interval;

        // Compute our view for every checkpoint in parallel and pair
        // each with its claimed root, then take the lowest-index
        // disagreement (deterministic regardless of completion order).
        let mismatch = stream::iter(game.intermediate_roots.iter().copied().zip(0u64..))
            .map(|(claimed, i)| async move {
                let block = Self::checkpoint_block(game.starting_l2_block, i + 1, interval);
                validator.compute_output_root(block).await.map(|computed| (i, claimed, computed))
            })
            .buffer_unordered(Self::SCAN_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter(|(_, claimed, computed)| claimed != computed)
            .min_by_key(|(i, _, _)| *i);

        // Every root matches what we compute: nothing to dispute.
        let Some((invalid_index, _, computed_root)) = mismatch else {
            return Ok(None);
        };

        let starting_root = Self::fetch_starting_root(game, validator, invalid_index).await?;
        let violation = Self::build(game, invalid_index, computed_root, starting_root, kind);
        info!(
            game = %game.address,
            invalid_index,
            ?kind,
            "violation detected"
        );
        Ok(Some(violation))
    }

    /// Compares only the contested checkpoint to what we compute (the
    /// ZK challenge targets exactly this index, others are out of
    /// scope). Returns a `FraudulentZkChallenge` violation when the
    /// proposed root at this index matches our view, `None` when
    /// it does not (the ZK challenger is legitimate from our vantage
    /// and will win on its own).
    async fn check_challenged_index(
        game: &GameInfo,
        validator: &dyn OutputValidator,
        challenged_index: u64,
    ) -> Result<Option<Self>, ValidationError> {
        let interval = game.intermediate_block_interval;
        let idx = usize::try_from(challenged_index).expect("challenged_index fits in usize");
        let proposed_root = game.intermediate_roots[idx];

        // Our computed view at the contested checkpoint.
        let end_block =
            Self::checkpoint_block(game.starting_l2_block, challenged_index + 1, interval);
        let computed_root = validator.compute_output_root(end_block).await?;

        // Proposed root diverges from our view: the ZK challenger
        // reached the same conclusion as us, let them resolve in
        // their favor on their own.
        if proposed_root != computed_root {
            return Ok(None);
        }

        // Proposed root matches our view: the ZK challenge looks
        // fraudulent from our vantage and must be countered.
        let starting_root = Self::fetch_starting_root(game, validator, challenged_index).await?;
        let violation = Self::build(
            game,
            challenged_index,
            computed_root,
            starting_root,
            ViolationKind::FraudulentZkChallenge { proposed_root },
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
    ) -> Result<B256, OutputRootError> {
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
        kind: ViolationKind,
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
            kind,
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
    Output(#[from] OutputRootError),

    /// Reading on-chain game state from the aggregate verifier failed.
    #[error(transparent)]
    Contract(#[from] ContractError),
}

/// Kind of violation detected on a dispute game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// Mismatch found while a TEE proof is attached. Resolved via
    /// the TEE-first dispute path, with ZK as fallback (see
    /// [`Violation::build_dispute_request`]). Reachable from
    /// [`ProvingState::TeeOnly`] and [`ProvingState::BothProven`].
    TeeWrong,
    /// Mismatch found while only a ZK proof is attached. Resolved
    /// by re-asserting our computed root via ZK (see
    /// [`Violation::build_dispute_request`]). Reachable from
    /// [`ProvingState::ZkOnly`].
    ZkWrong,
    /// A pending ZK challenge targets a checkpoint where the
    /// proposed root matches our computed view. Resolved by
    /// re-asserting `proposed_root` via ZK (see
    /// [`Violation::build_dispute_request`]). Reachable from both
    /// [`ProvingState::UnderChallenge`] and
    /// [`ProvingState::TeeNullifiedDuringChallenge`].
    FraudulentZkChallenge {
        /// Proposed root at `invalid_index` (CWIA-immutable). The
        /// counter-nullify ZK proof must commit to this exact value.
        proposed_root: B256,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockAggregateVerifier, MockGameState, MockOutputValidator, addr};

    const STARTING_BLOCK: u64 = 100;
    const INTERVAL: u64 = 5;

    fn root(i: u8) -> B256 {
        B256::repeat_byte(i)
    }

    fn build_game(intermediate_roots: Vec<B256>, proving_state: ProvingState) -> GameInfo {
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
            proving_state,
        }
    }

    fn build_state(proving_state: ProvingState) -> MockGameState {
        let (tee, zk, c) = match proving_state {
            ProvingState::TeeOnly => (addr(1), Address::ZERO, 0),
            ProvingState::ZkOnly => (Address::ZERO, addr(2), 0),
            ProvingState::BothProven => (addr(1), addr(2), 0),
            ProvingState::UnderChallenge { challenged_index } => {
                (addr(1), addr(2), challenged_index + 1)
            }
            ProvingState::TeeNullifiedDuringChallenge { challenged_index } => {
                (Address::ZERO, addr(2), challenged_index + 1)
            }
            ProvingState::AllNullified => (Address::ZERO, Address::ZERO, 0),
        };
        MockGameState::in_progress(tee, zk, c)
    }

    /// Builds a `(GameInfo, validator, verifier)` triple where the
    /// validator returns `expected_at[i]` for block `STARTING_BLOCK +
    /// (i+1)*INTERVAL` and the verifier reflects `proving_state`.
    fn fixture(
        intermediate_roots: Vec<B256>,
        expected_at: &[B256],
        proving_state: ProvingState,
    ) -> (GameInfo, MockOutputValidator, MockAggregateVerifier) {
        let game = build_game(intermediate_roots, proving_state);
        let validator = MockOutputValidator::new();
        for (i, r) in expected_at.iter().enumerate() {
            let block = STARTING_BLOCK + (i as u64 + 1) * INTERVAL;
            validator.set(block, *r);
        }
        let verifier = MockAggregateVerifier::new();
        verifier.set_game(game.address, build_state(proving_state));
        (game, validator, verifier)
    }

    #[tokio::test]
    async fn tee_only_all_correct_returns_none() {
        let r0 = root(10);
        let r1 = root(11);
        let r2 = root(12);
        let (game, v, c) = fixture(vec![r0, r1, r2], &[r0, r1, r2], ProvingState::TeeOnly);
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tee_only_index_2_wrong_returns_tee_wrong() {
        let r0 = root(10);
        let r1 = root(11);
        let r2 = root(12);
        let expected_r2 = root(99);
        let (game, v, c) = fixture(vec![r0, r1, r2], &[r0, r1, expected_r2], ProvingState::TeeOnly);

        let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
        assert_eq!(violation.kind, ViolationKind::TeeWrong);
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
        let (game, v, c) = fixture(vec![r0], &[expected_r0], ProvingState::ZkOnly);
        // For invalid_index == 0, detect fetches the validator's
        // output at starting_l2_block as the predecessor root.
        v.set(STARTING_BLOCK, starting_expected);

        let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
        assert_eq!(violation.kind, ViolationKind::ZkWrong);
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
        let (game, v, c) = fixture(vec![r0, r1], &[r0, r1], ProvingState::ZkOnly);
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn both_proven_index_1_wrong_returns_tee_wrong() {
        let r0 = root(10);
        let r1 = root(11);
        let expected_r1 = root(99);
        let (game, v, c) = fixture(vec![r0, r1], &[r0, expected_r1], ProvingState::BothProven);

        let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
        // BothProven nullifies TEE first; ZK gets caught on a later scan.
        assert_eq!(violation.kind, ViolationKind::TeeWrong);
        assert_eq!(violation.invalid_index, 1);
        assert_eq!(violation.computed_root, expected_r1);
    }

    #[tokio::test]
    async fn both_proven_all_correct_returns_none() {
        let r0 = root(10);
        let (game, v, c) = fixture(vec![r0], &[r0], ProvingState::BothProven);
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn under_challenge_with_matching_proposed_returns_fraudulent() {
        let r0 = root(10);
        let r1 = root(11);
        let challenged_index = 1;
        let (game, v, c) =
            fixture(vec![r0, r1], &[r0, r1], ProvingState::UnderChallenge { challenged_index });

        let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
        match violation.kind {
            ViolationKind::FraudulentZkChallenge { proposed_root } => {
                assert_eq!(proposed_root, r1);
            }
            other => panic!("expected FraudulentZkChallenge, got {other:?}"),
        }
        assert_eq!(violation.invalid_index, 1);
        assert_eq!(violation.computed_root, r1);
        assert_eq!(violation.starting_root, r0);
    }

    #[tokio::test]
    async fn under_challenge_with_diverging_proposed_returns_none() {
        let r0 = root(10);
        let proposed_r1 = root(11);
        let computed_r1 = root(99);
        let challenged_index = 1;
        let (game, v, c) = fixture(
            vec![r0, proposed_r1],
            &[r0, computed_r1],
            ProvingState::UnderChallenge { challenged_index },
        );
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tee_nullified_with_matching_proposed_returns_fraudulent() {
        let r0 = root(10);
        let r1 = root(11);
        let challenged_index = 1;
        let (game, v, c) = fixture(
            vec![r0, r1],
            &[r0, r1],
            ProvingState::TeeNullifiedDuringChallenge { challenged_index },
        );

        let violation = Violation::detect(&game, &v, &c).await.unwrap().unwrap();
        match violation.kind {
            ViolationKind::FraudulentZkChallenge { proposed_root } => {
                assert_eq!(proposed_root, r1);
            }
            other => panic!("expected FraudulentZkChallenge, got {other:?}"),
        }
        assert_eq!(violation.invalid_index, 1);
        assert_eq!(violation.computed_root, r1);
        assert_eq!(violation.starting_root, r0);
    }

    #[tokio::test]
    async fn tee_nullified_with_diverging_proposed_returns_none() {
        let r0 = root(10);
        let proposed_r1 = root(11);
        let computed_r1 = root(99);
        let challenged_index = 1;
        let (game, v, c) = fixture(
            vec![r0, proposed_r1],
            &[r0, computed_r1],
            ProvingState::TeeNullifiedDuringChallenge { challenged_index },
        );
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn all_nullified_returns_none() {
        let game = build_game(vec![root(10)], ProvingState::AllNullified);
        let v = MockOutputValidator::new();
        let c = MockAggregateVerifier::new();
        c.set_game(game.address, build_state(ProvingState::AllNullified));
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolved_game_returns_none() {
        // Game has resolved on-chain (DefenderWins) but the prover
        // tuple still classifies as TeeOnly with matching roots.
        let r0 = root(10);
        let (game, v, c) = fixture(vec![r0], &[r0], ProvingState::TeeOnly);
        let mut state = build_state(ProvingState::TeeOnly);
        state.status = base_proof_contracts::GameStatus::DefenderWins;
        c.set_game(game.address, state);
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unreachable_state_returns_none() {
        // tee=0, zk=0, countered>0 is unreachable per classify().
        let game = build_game(vec![root(10)], ProvingState::TeeOnly);
        let v = MockOutputValidator::new();
        let c = MockAggregateVerifier::new();
        c.set_game(game.address, MockGameState::in_progress(Address::ZERO, Address::ZERO, 7));
        assert!(Violation::detect(&game, &v, &c).await.unwrap().is_none());
    }
}
