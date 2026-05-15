//! Per-game worker pipeline: [`run_game_worker`] and the shared
//! [`WorkerDeps`] it consumes.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_contracts::AggregateVerifierClient;
use base_zk_client::ZkProofProvider;
use derive_more::Debug;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    ChallengerMetrics, DisputeRequest, GameInfo, OutputValidator, TeeProofProvider, Violation,
};

/// Read-only handles and config shared across every game-worker task.
///
/// Cloned via `Arc<WorkerDeps>` when spawning workers, so spawning is
/// cheap and all tasks observe the same configured services.
#[derive(Debug)]
pub struct WorkerDeps {
    /// L2 output root computer used by [`Violation::detect`].
    #[debug(skip)]
    pub validator: Arc<dyn OutputValidator>,
    /// Aggregate verifier read by [`Violation::detect`].
    #[debug(skip)]
    pub verifier: Arc<dyn AggregateVerifierClient>,
    /// ZK proving service. Generates SNARK proofs over disputed L2
    /// block ranges; used by every dispute path that needs a
    /// cryptographic on-chain proof.
    #[debug(skip)]
    pub zk_prover: Arc<dyn ZkProofProvider>,
    /// TEE proving service. Signs attestations over disputed L2
    /// block ranges; used as a fast path when the dispute can be
    /// settled by a TEE signature, with ZK as the fallback.
    #[debug(skip)]
    pub tee_prover: Arc<dyn TeeProofProvider>,
    /// Static worker config.
    pub config: WorkerConfig,
}

impl WorkerDeps {
    /// Bundles the read clients, proving services and config for
    /// sharing across workers.
    pub fn new(
        validator: Arc<dyn OutputValidator>,
        verifier: Arc<dyn AggregateVerifierClient>,
        zk_prover: Arc<dyn ZkProofProvider>,
        tee_prover: Arc<dyn TeeProofProvider>,
        config: WorkerConfig,
    ) -> Self {
        Self { validator, verifier, zk_prover, tee_prover, config }
    }
}

/// Per-worker configuration. `Copy` so it flows through async boundaries
/// without atomics or clones.
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// Address that will sign and submit dispute transactions on L1.
    /// Forwarded to the ZK service so the SNARK journal commits to
    /// the same `msg.sender` the contract will see.
    pub sender_address: Address,
    /// Number of additional ZK proof attempts after the first one.
    /// Total attempts equals `max_proof_retries + 1`.
    pub max_proof_retries: u32,
    /// Sleep between successive ZK proof status polls while waiting
    /// for a job to reach a terminal state.
    pub proof_poll_interval: Duration,
    /// Per-attempt deadline for ZK proving. When exceeded the attempt
    /// is abandoned and a retry, if any remains, is initiated.
    pub max_proof_duration: Duration,
}

/// Drives a single game through validate / prove / submit and exits.
/// Each terminal branch is recorded on
/// [`ChallengerMetrics::game_worker_outcome_total`].
pub async fn run_game_worker(
    game: GameInfo,
    deps: Arc<WorkerDeps>,
    submit_tx: mpsc::Sender<DisputeRequest>,
) {
    let game_address = game.address;

    // Validate: re-fetch live state and compare to claimed roots.
    let violation = match Violation::detect(&game, &*deps.validator, &*deps.verifier).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            debug!(game = %game_address, "no violation detected");
            ChallengerMetrics::game_worker_outcome_total(
                ChallengerMetrics::GAME_WORKER_OUTCOME_NO_VIOLATION,
            )
            .increment(1);
            return;
        }
        Err(e) => {
            warn!(game = %game_address, error = %e, "validation failed");
            ChallengerMetrics::game_worker_outcome_total(
                ChallengerMetrics::GAME_WORKER_OUTCOME_VALIDATION_ERROR,
            )
            .increment(1);
            return;
        }
    };

    // Prove: produce the proof bytes that back the dispute action.
    let request = match violation.dispute_request(&deps).await {
        Ok(r) => r,
        Err(e) => {
            warn!(game = %game_address, error = %e, "dispute proof generation failed");
            ChallengerMetrics::game_worker_outcome_total(
                ChallengerMetrics::GAME_WORKER_OUTCOME_PROOF_ERROR,
            )
            .increment(1);
            return;
        }
    };

    // Submit: hand off to the SubmissionTask.
    if submit_tx.send(request).await.is_err() {
        warn!(game = %game_address, "submission channel closed; dropping request");
        ChallengerMetrics::game_worker_outcome_total(
            ChallengerMetrics::GAME_WORKER_OUTCOME_SEND_DROPPED,
        )
        .increment(1);
        return;
    }

    info!(game = %game_address, "dispute request dispatched");
    ChallengerMetrics::game_worker_outcome_total(ChallengerMetrics::GAME_WORKER_OUTCOME_DISPATCHED)
        .increment(1);
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;
    use crate::{
        DisputeAction, GameSituation,
        test_utils::{
            MockAggregateVerifier, MockGameState, MockOutputValidator, MockTeeProofProvider,
            MockZkProofProvider, addr,
        },
    };

    const STARTING_BLOCK: u64 = 100;
    const INTERVAL: u64 = 5;

    fn root(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn checkpoint_block(i: u64) -> u64 {
        STARTING_BLOCK + (i + 1) * INTERVAL
    }

    fn config() -> WorkerConfig {
        WorkerConfig {
            sender_address: addr(0xB2),
            max_proof_retries: 0,
            proof_poll_interval: Duration::from_millis(1),
            max_proof_duration: Duration::from_secs(1),
        }
    }

    /// Builds a [`GameInfo`] in the requested situation. Roots are the
    /// values claimed on-chain at each checkpoint.
    fn game_info(intermediate_roots: Vec<B256>, situation: GameSituation) -> GameInfo {
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

    /// Mock state for `(tee, zk, countered)` matching `situation`.
    fn game_state(situation: GameSituation) -> MockGameState {
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

    /// Bundles the four mocks needed by [`WorkerDeps`].
    struct Mocks {
        validator: Arc<MockOutputValidator>,
        verifier: Arc<MockAggregateVerifier>,
        zk: Arc<MockZkProofProvider>,
        tee: Arc<MockTeeProofProvider>,
    }

    impl Mocks {
        fn new() -> Self {
            Self {
                validator: Arc::new(MockOutputValidator::new()),
                verifier: Arc::new(MockAggregateVerifier::new()),
                zk: Arc::new(MockZkProofProvider::new()),
                tee: Arc::new(MockTeeProofProvider::new()),
            }
        }

        fn deps(&self) -> Arc<WorkerDeps> {
            Arc::new(WorkerDeps::new(
                Arc::<MockOutputValidator>::clone(&self.validator),
                Arc::<MockAggregateVerifier>::clone(&self.verifier),
                Arc::<MockZkProofProvider>::clone(&self.zk),
                Arc::<MockTeeProofProvider>::clone(&self.tee),
                config(),
            ))
        }
    }

    /// Programs `mocks` so that `Violation::detect` returns `None` for
    /// `game`: classifier sees `situation`, every checkpoint matches
    /// the on-chain claim.
    fn arrange_no_violation(mocks: &Mocks, game: &GameInfo, situation: GameSituation) {
        mocks.verifier.set_game(game.address, game_state(situation));
        for (i, root) in game.intermediate_roots.iter().enumerate() {
            mocks.validator.set(checkpoint_block(i as u64), *root);
        }
    }

    #[tokio::test]
    async fn no_violation_does_not_send() {
        let mocks = Mocks::new();
        let game = game_info(vec![root(10), root(11)], GameSituation::ZkOnly);
        arrange_no_violation(&mocks, &game, GameSituation::ZkOnly);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn violation_with_successful_proof_sends_request() {
        let mocks = Mocks::new();
        // ZkOnly with index-0 mismatch routes through the ZK-only proving
        // path: prove() submits one job and returns the receipt bytes.
        let on_chain = root(10);
        let computed = root(99);
        let game = game_info(vec![on_chain], GameSituation::ZkOnly);
        mocks.verifier.set_game(game.address, game_state(GameSituation::ZkOnly));
        mocks.validator.set(checkpoint_block(0), computed);
        // index 0: detect also fetches the predecessor root from the
        // game's starting block.
        mocks.validator.set(STARTING_BLOCK, root(50));
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_succeeded(vec![0xAA, 0xBB]);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game.clone(), mocks.deps(), tx).await;

        let req = rx.try_recv().expect("a request was sent");
        assert_eq!(req.game_address, game.address);
        assert!(matches!(req.action, DisputeAction::NullifyZk { .. }));
    }

    #[tokio::test]
    async fn validation_error_does_not_send() {
        let mocks = Mocks::new();
        // Verifier has no entry for this game: tee_prover() returns a
        // ContractError, which surfaces as ValidationError::Contract.
        let game = game_info(vec![root(10)], GameSituation::ZkOnly);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn proof_error_does_not_send() {
        let mocks = Mocks::new();
        let on_chain = root(10);
        let computed = root(99);
        let game = game_info(vec![on_chain], GameSituation::ZkOnly);
        mocks.verifier.set_game(game.address, game_state(GameSituation::ZkOnly));
        mocks.validator.set(checkpoint_block(0), computed);
        mocks.validator.set(STARTING_BLOCK, root(50));
        // ZK prover returns a permanent failure: with max_proof_retries=0
        // the first failure is also the last.
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_failed(Some("simulated".into()));
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropped_receiver_does_not_panic() {
        let mocks = Mocks::new();
        let on_chain = root(10);
        let computed = root(99);
        let game = game_info(vec![on_chain], GameSituation::ZkOnly);
        mocks.verifier.set_game(game.address, game_state(GameSituation::ZkOnly));
        mocks.validator.set(checkpoint_block(0), computed);
        mocks.validator.set(STARTING_BLOCK, root(50));
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_succeeded(vec![0x42]);
        let (tx, rx) = mpsc::channel(4);
        drop(rx);

        // Worker logs and returns; no panic, no hang.
        run_game_worker(game, mocks.deps(), tx).await;
    }
}
