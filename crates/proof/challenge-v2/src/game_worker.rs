//! Per-game worker: [`run_game_worker`] runs one detect / dispute /
//! submit pass per spawn. Re-spawn between scan ticks is the pool's
//! job (driven by [`crate::GameDiscovery`] re-emitting the game).

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_contracts::AggregateVerifierClient;
use base_zk_client::ZkProofProvider;
use derive_more::Debug;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, info, warn};

use crate::{DisputeRequest, GameInfo, OutputValidator, TeeProofProvider, Violation};

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
    /// Caps concurrent [`Violation::detect`] runs so a burst of
    /// workers cannot saturate the L1/L2 RPC pool.
    #[debug(skip)]
    pub detect_semaphore: Arc<Semaphore>,
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
        detect_semaphore: Arc<Semaphore>,
        config: WorkerConfig,
    ) -> Self {
        Self { validator, verifier, zk_prover, tee_prover, detect_semaphore, config }
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

/// Runs one detect / dispute / submit pass for `game` and exits.
/// Each branch logs its outcome; the next iteration is driven by the
/// scanner re-emitting the game.
pub async fn run_game_worker(
    game: GameInfo,
    deps: Arc<WorkerDeps>,
    submit_tx: mpsc::Sender<DisputeRequest>,
) {
    let address = game.address;

    // Acquire a permit for the detect phase only: detect is RPC-heavy
    // (status + prover tuple + per-checkpoint output roots), and the
    // pool can spawn many workers at once.
    let violation = {
        let _permit =
            deps.detect_semaphore.acquire().await.expect("detect_semaphore is never closed");

        match Violation::detect(&game, &*deps.validator, &*deps.verifier).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                debug!(game = %address, "no violation");
                return;
            }
            Err(e) => {
                warn!(game = %address, error = %e, "validation failed");
                return;
            }
        }
    };

    let request = match violation.build_dispute_request(&deps).await {
        Ok(r) => r,
        Err(e) => {
            warn!(game = %address, error = %e, "dispute proof failed");
            return;
        }
    };

    if submit_tx.send(request).await.is_err() {
        warn!(game = %address, "submission channel closed");
        return;
    }

    info!(game = %address, "dispute request dispatched");
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;
    use crate::{
        DisputeAction, ProvingState,
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

    fn game_info(intermediate_roots: Vec<B256>, proving_state: ProvingState) -> GameInfo {
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
                Arc::new(Semaphore::new(8)),
                config(),
            ))
        }
    }

    /// Programs the verifier so the game classifies as `ZkOnly` with
    /// every checkpoint matching the on-chain claim (no violation).
    fn arrange_no_violation(mocks: &Mocks, game: &GameInfo) {
        mocks
            .verifier
            .set_game(game.address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
        for (i, r) in game.intermediate_roots.iter().enumerate() {
            mocks.validator.set(checkpoint_block(i as u64), *r);
        }
    }

    /// Programs the verifier + validator so detect returns a `ZkWrong`
    /// violation at index 0 and the ZK prover succeeds.
    fn arrange_zk_only_violation(mocks: &Mocks, game: &GameInfo) {
        mocks
            .verifier
            .set_game(game.address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
        mocks.validator.set(checkpoint_block(0), root(99));
        mocks.validator.set(STARTING_BLOCK, root(50));
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_succeeded(vec![0xAA, 0xBB]);
    }

    #[tokio::test]
    async fn no_violation_does_not_send() {
        let mocks = Mocks::new();
        let game = game_info(vec![root(10), root(11)], ProvingState::ZkOnly);
        arrange_no_violation(&mocks, &game);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn violation_with_successful_proof_sends_request() {
        let mocks = Mocks::new();
        let game = game_info(vec![root(10)], ProvingState::ZkOnly);
        arrange_zk_only_violation(&mocks, &game);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game.clone(), mocks.deps(), tx).await;

        let req = rx.try_recv().expect("a request was sent");
        assert_eq!(req.game_address, game.address);
        assert!(matches!(req.action, DisputeAction::NullifyZk { .. }));
    }

    #[tokio::test]
    async fn validation_error_does_not_send() {
        let mocks = Mocks::new();
        // Verifier has no entry for this game: its reads return
        // ContractError, surfaced as ValidationError::Contract.
        let game = game_info(vec![root(10)], ProvingState::ZkOnly);
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn proof_error_does_not_send() {
        let mocks = Mocks::new();
        let game = game_info(vec![root(10)], ProvingState::ZkOnly);
        mocks
            .verifier
            .set_game(game.address, MockGameState::in_progress(Address::ZERO, addr(2), 0));
        mocks.validator.set(checkpoint_block(0), root(99));
        mocks.validator.set(STARTING_BLOCK, root(50));
        // ZK prover returns a permanent failure.
        mocks.zk.push_prove_ok();
        mocks.zk.push_get_failed(Some("simulated".into()));
        let (tx, mut rx) = mpsc::channel(4);

        run_game_worker(game, mocks.deps(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropped_receiver_does_not_panic() {
        let mocks = Mocks::new();
        let game = game_info(vec![root(10)], ProvingState::ZkOnly);
        arrange_zk_only_violation(&mocks, &game);
        let (tx, rx) = mpsc::channel(4);
        drop(rx);

        // No panic, returns cleanly after logging the channel-closed warn.
        run_game_worker(game, mocks.deps(), tx).await;
    }
}
