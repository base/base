//! Drives a real challenger binary against a throwaway fork of the target L1.

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use alloy_consensus::Transaction as _;
use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, B256, Bytes, U256, hex};
use alloy_provider::{Provider, RootProvider, network::TransactionResponse as _};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
    GameStatus, decode_dispute_calldata, describe_revert, encode_nullify_calldata,
};
use base_proof_rpc::L2HttpProvider;
use base_proof_submission::{AggregateProofSubmitter, ProofSubmissionError};
use base_prover_service_protocol::ZkBackend;
use base_tx_manager::{
    NoopTxMetrics, SignerConfig, SimpleTxManager, TxCandidate, TxManager, TxManagerConfig,
    TxManagerError,
};
use base_zk_fork_dispute::{Checkpoint, Config as ForkConfig};
use clap::Parser;
use eyre::{Context, Result, bail, ensure, eyre};
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    config::{Config, ProverMode, Scenario},
    metrics::Scrape,
    mock_prover::MockProver,
    mock_verifier,
};

/// Wei granted to each throwaway account on the fork. Orders of magnitude more
/// than a dispute costs, and worthless outside the pod.
const FUNDING_WEI: u128 = 100_000_000_000_000_000_000;

/// The root Path 3 staging "proves" to drop the TEE proof of a still-valid
/// game. Anything other than the stored root passes `_checkIntermediateRoot`,
/// and the verifier is mocked for that one call.
const PATH3_STAGING_ROOT: B256 = B256::repeat_byte(0xde);

/// Counters that must stay at zero for as long as every game on the fork is
/// valid. Checked absolutely at the baseline and as a delta over the window.
const DISPUTE_COUNTERS: [&str; 3] = [
    "base_challenger_games_invalid_total",
    "base_challenger_nullify_tx_submitted_total",
    "base_challenger_challenge_tx_submitted_total",
];

/// Count series of the challenger's validation-latency histogram.
///
/// Recorded once per call to the validator's `validate_output_roots`, which is
/// reached once per candidate game.
const VALIDATIONS: &str = "base_challenger_validation_latency_seconds_count";

/// Failed validations, one per failed [`VALIDATIONS`] call.
///
/// `validate_output_roots` records its latency from a drop guard, so the
/// histogram counts attempts rather than successes. It increments this counter
/// exactly once on the way out of a failure — the `?` returns on the first
/// error it observes — so the difference of the two is the number of games the
/// challenger actually validated.
const VALIDATION_ERRORS: &str = "base_challenger_validation_errors_total";

/// A game the challenger has been observed to accept, plus its root count.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    address: Address,
    root_count: u64,
}

/// How Path 1 landed. Decides which property the settle window below proves.
#[derive(Debug, Clone, Copy)]
enum Path1Outcome {
    TeeNullify,
    ZkChallenge,
}

/// Everything the challenger can change about a game.
///
/// The challenger only ever nullifies or challenges, and both show up here, so
/// an unchanged triple means the challenger did not act on the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GameState {
    tee_prover: Address,
    zk_prover: Address,
    countered_index: u64,
}

/// Stage of the run, emitted as the `phase` field on every outcome log.
///
/// Datadog sees each log line in isolation, so a run is only as readable as its
/// fields: `@data.message.fields.phase:path3` finds every Path 3 outcome across
/// every run, and `@data.message.fields.verdict:fail` finds the failures without
/// grepping message strings. The names are stable API — dashboards and monitors
/// filter on them, so renaming one breaks whatever watches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Fork spawned, games selected, bystanders snapshotted.
    Setup,
    /// The positive case: every game on the fork is valid and must stay so.
    QuietWindow,
    /// Path 1, an invalid TEE-only proposal.
    Path1,
    /// Path 2's first half: a legitimate challenge must be left standing.
    Path2Skip,
    /// Path 2's second half: a fraudulent challenge must be nullified.
    Path2Dispute,
    /// Path 3, an invalid ZK-only proposal.
    Path3,
    /// Path 4, an invalid dual-proof proposal.
    Path4,
    /// The collateral-damage check over games never under test.
    Bystanders,
}

impl Phase {
    /// Returns the stable field value for this phase.
    ///
    /// Kebab-case and lowercase so a Datadog facet needs no normalisation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::QuietWindow => "quiet-window",
            Self::Path1 => "path1",
            Self::Path2Skip => "path2-skip",
            Self::Path2Dispute => "path2-dispute",
            Self::Path3 => "path3",
            Self::Path4 => "path4",
            Self::Bystanders => "bystanders",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a phase, emitted as the `verdict` field alongside `phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The phase asserted its claim and the claim held.
    Pass,
    /// The phase could not assert its claim, for a reason that is not the
    /// challenger's fault. Coverage was lost, not violated.
    Skip,
}

impl Verdict {
    /// Returns the stable field value for this verdict.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Skip => "skip",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Behavioural end-to-end test of the challenger.
///
/// See the crate README for the full argument; the short version is that the
/// fork is built from a real chain and the games under test were created and
/// verified on that chain. In the default `mock` prover mode the proofs and
/// the verifiers that check them are stubbed, and the driver asserts instead
/// what the verifier would have checked: that every dispute proved the
/// canonical root at the corrupted index. In `real` mode nothing is stubbed.
#[derive(Debug)]
pub struct ChallengerE2e;

impl ChallengerE2e {
    /// Runs the test to completion. An `Ok` return means the challenger passed.
    pub async fn run() -> Result<()> {
        let mut config = Config::parse();

        // Two distinct accounts: A (driver) signs setup only, B is the
        // challenger. Both are generated per run and never leave the pod.
        let driver = PrivateKeySigner::random();
        let challenger = PrivateKeySigner::random();

        // Emitted before anything can fail, so even a run that dies in setup
        // says which scenario it was and which key to attribute disputes to.
        // Datadog shows each line alone; without this, the scenario has to be
        // inferred from which later messages happen to appear.
        info!(
            phase = %Phase::Setup,
            scenario = ?config.scenario,
            prover = ?config.prover,
            challenger_address = %challenger.address(),
            driver_address = %driver.address(),
            quiet_window = ?config.quiet_window,
            dispute_timeout = ?config.dispute_timeout,
            game_type = config.game_type,
            "starting scenario"
        );

        // Held until the end of run(); the fork dies with this binding.
        let anvil = Self::spawn_fork(&config)?;
        let fork_url = anvil.endpoint_url();
        let provider: RootProvider = RootProvider::new_http(fork_url.clone());
        config.fork_block = provider.get_block_number().await?;
        Self::fund(&provider, &[driver.address(), challenger.address()]).await?;

        // Up before anything asks for a proof: the dual-proof staging below
        // does, and so does the challenger once released. Held until the end of
        // run(), like the fork. Every later use of `zk_rpc_url` — the driver's
        // staging and the challenger's env file — now points at the mock.
        let _mock_prover = match config.prover {
            ProverMode::Mock => {
                let mock = MockProver::start(config.mock_proving_time).await?;
                config.zk_rpc_url = mock.url().clone();
                Some(mock)
            }
            ProverMode::Real => None,
        };

        let factory = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            provider.clone(),
        );
        let verifier = AggregateVerifierContractClient::new(provider.clone());
        let anchor_registry = AnchorStateRegistryContractClient::new(
            config.anchor_state_registry_addr,
            provider.clone(),
        );

        // Chosen before the challenger boots so the positive case below is
        // measured against a fork that already contains the target games.
        let (game_a, game_b) =
            Self::select_games(&config, &factory, &verifier, &anchor_registry).await?;
        if config.prover == ProverMode::Mock {
            Self::install_mock_verifiers(
                &config,
                &provider,
                &verifier,
                [game_a.address, game_b.address],
            )
            .await?;
        }

        // Taken before the challenger boots. Every dispute assertion below is
        // scoped to A or B, so without this a challenger that also disputes
        // games it was never given would pass the run.
        let bystanders = Self::snapshot_bystanders(
            &config,
            &factory,
            &verifier,
            [game_a.address, game_b.address],
        )
        .await?;

        if matches!(config.scenario, Scenario::All | Scenario::Path3) {
            Self::stage_dual_proof(&config, &fork_url, &verifier, &provider, &driver, game_b)
                .await?;
        }

        Self::release_challenger(&config.env_file, &fork_url, &config.zk_rpc_url, &challenger)?;
        Self::await_first_scan(&config).await?;

        Self::assert_quiet_on_valid_games(&config).await?;

        // After the quiet window, not before it. B is a valid dual-proof game
        // until `stage_path3` patches it, so this scenario gets the same
        // positive case as every other one: a challenger that disputes valid
        // games fails above rather than reaching Path 3 at all.
        if config.scenario == Scenario::Path3 {
            // Game A is never corrupted in this scenario, so it is a valid game
            // the challenger must leave alone — but `snapshot_bystanders`
            // excludes both games under test. Watching it here is what makes
            // the collateral-damage check cover it.
            let mut untouched = bystanders;
            untouched
                .push((game_a.address, Self::read_game_state(&verifier, game_a.address).await?));

            // Zero, because `assert_quiet_on_valid_games` above asserts these
            // counters are still absolutely zero once the first scan completes.
            let submitted = Self::disputes_submitted(&config).await?;

            let (nonce, dual_before, checkpoint) = Self::stage_path3(
                &config,
                &fork_url,
                &verifier,
                &provider,
                &driver,
                &challenger,
                game_b,
            )
            .await?;

            // The ZK proof must go, whichever route cleared it. A timeout here
            // fails the run: the game is still invalid, and an E2E that reports
            // success over an undisputed invalid game is worse than no E2E.
            Self::await_path3(
                &config,
                &verifier,
                &provider,
                &challenger,
                game_b,
                nonce,
                submitted,
                dual_before,
            )
            .await?;
            Self::assert_disputes_prove_canonical(
                &config,
                &provider,
                &challenger,
                game_b.address,
                &checkpoint,
                Phase::Path3,
            )
            .await?;

            info!(window = ?config.quiet_window, "observing the fork after Path 3");
            tokio::time::sleep(config.quiet_window).await;

            let after = Self::disputes_submitted(&config).await?;
            ensure!(
                after <= submitted + 1.0,
                "the challenger submitted {} dispute(s) in the {:?} after Path 3 completed; \
                 nothing on the fork was disputable, and a dispute that reverts leaves every \
                 game state untouched",
                after - submitted - 1.0,
                config.quiet_window
            );

            Self::assert_bystanders_untouched(&verifier, &untouched).await?;

            Self::log_scenario_complete(
                &config,
                &[Phase::QuietWindow, Phase::Path3, Phase::Bystanders],
            );
            return Ok(());
        }

        let (path1, checkpoint) =
            Self::run_path1(&config, &fork_url, &verifier, &provider, &driver, &challenger, game_a)
                .await?;
        Self::assert_game_a_settled(&config, &verifier, &provider, &challenger, game_a, path1)
            .await?;
        if config.scenario == Scenario::Path1Path2 {
            ensure!(
                matches!(path1, Path1Outcome::ZkChallenge),
                "Path 2 dispute requires Path 1 to land as a ZK challenge"
            );
            Self::run_path2(
                &config,
                Self::fork_config(&config, &fork_url, &driver, game_a),
                &verifier,
                &provider,
                &challenger,
                game_a,
                checkpoint,
            )
            .await?;
            Self::assert_bystanders_untouched(&verifier, &bystanders).await?;
            Self::log_scenario_complete(
                &config,
                &[
                    Phase::QuietWindow,
                    Phase::Path1,
                    Phase::Path2Skip,
                    Phase::Path2Dispute,
                    Phase::Bystanders,
                ],
            );
            return Ok(());
        }
        let path3_in_situ =
            Self::run_path4(&config, &fork_url, &verifier, &provider, &driver, &challenger, game_b)
                .await?;

        Self::assert_bystanders_untouched(&verifier, &bystanders).await?;

        // Built from what ran: `assert_game_a_settled` asserts the Path 2 skip
        // only when Path 1 landed as a ZK challenge, and Path 4 reaches Path 3
        // only on its TEE-first branch.
        let mut asserted = vec![Phase::QuietWindow, Phase::Path1];
        if matches!(path1, Path1Outcome::ZkChallenge) {
            asserted.push(Phase::Path2Skip);
        }
        asserted.push(Phase::Path4);
        if path3_in_situ {
            asserted.push(Phase::Path3);
        }
        asserted.push(Phase::Bystanders);
        Self::log_scenario_complete(&config, &asserted);
        Ok(())
    }

    /// Records which phases a scenario asserted, as one line.
    ///
    /// The per-phase logs say what happened; this says what the run *claimed to
    /// cover*, which is the question a dashboard asks. Without it, absence of a
    /// Path 3 log is ambiguous between "not part of this scenario" and "skipped".
    fn log_scenario_complete(config: &Config, asserted: &[Phase]) {
        let asserted: Vec<&str> = asserted.iter().map(|phase| phase.as_str()).collect();
        info!(
            scenario = ?config.scenario,
            phases_asserted = ?asserted,
            "scenario complete"
        );
    }

    fn spawn_fork(config: &Config) -> Result<AnvilInstance> {
        // Host only. Provider URLs routinely carry the API key in the path or
        // the query string, and this log ships to a shared aggregator.
        info!(
            fork_source = config.l1_eth_rpc.host_str().unwrap_or("<no host>"),
            port = config.anvil_port,
            "spawning L1 fork"
        );
        Anvil::new()
            .fork(config.l1_eth_rpc.as_str())
            .port(config.anvil_port)
            .timeout(u64::try_from(config.startup_timeout.as_millis()).unwrap_or(u64::MAX))
            // A cold fork issues a burst of archive reads; the default client-side
            // throttle turns that into a startup timeout.
            .arg("--no-rate-limit")
            .try_spawn()
            .context("failed to spawn anvil; the binary must be on PATH")
    }

    async fn fund(provider: &RootProvider, addresses: &[Address]) -> Result<()> {
        for address in addresses {
            provider
                .client()
                .request::<_, ()>("anvil_setBalance", (address, U256::from(FUNDING_WEI)))
                .await
                .with_context(|| format!("anvil_setBalance failed for {address}"))?;
        }
        Ok(())
    }

    /// Picks the two newest in-progress TEE-only games the challenger will
    /// classify as disputable once their roots stop matching L2.
    ///
    /// Game A is Path 1 (and Path 2 skip if that lands as a ZK challenge).
    /// Game B is Path 4→3. Scanning newest-first also keeps the corrupted
    /// range recent, which matters because the L2 RPC is a live node and may
    /// have pruned the state behind an older game.
    async fn select_games(
        config: &Config,
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
        anchor_registry: &AnchorStateRegistryContractClient,
    ) -> Result<(Candidate, Candidate)> {
        let game_count = factory.game_count().await?;
        if game_count == 0 {
            bail!("factory {} has no games on the fork", config.dispute_game_factory_addr);
        }
        let floor = game_count.saturating_sub(config.game_lookback);

        // The challenger scans from one past the anchor game's factory index,
        // so anything at or before it is invisible to the challenger no matter
        // what state it is in. Walking newest-first means stopping at the
        // anchor is the whole lower bound.
        let anchor_game = anchor_registry.anchor_snapshot().await?.anchor_game;

        // The aggregation program the implementation the factory currently points
        // at verifies against. Every hash on an `AggregateVerifier` is
        // `immutable`, so a verification-key rotation deploys a new implementation
        // and leaves existing clones pinned to the old one. Proving against such a
        // clone spends a full SNARK and then reverts `InvalidProof()` at
        // submission. This only keeps selection consistent with the factory: it
        // says nothing about whether the prover-service builds the same program,
        // which is a separate failure with the same symptom.
        let implementation = factory.game_impls(config.game_type).await?;
        ensure!(
            implementation != Address::ZERO,
            "no AggregateVerifier implementation registered for game type {}",
            config.game_type
        );
        let expected_hash =
            verifier.zk_aggregate_hash(implementation).await.with_context(|| {
                format!("failed to read ZK_AGGREGATE_HASH from implementation {implementation}")
            })?;
        info!(
            phase = %Phase::Setup,
            implementation = %implementation,
            zk_aggregate_hash = %expected_hash,
            "read the aggregation program the prover must match"
        );

        let mut stale = 0usize;
        let mut selected = Vec::with_capacity(2);
        for index in (floor..game_count).rev() {
            let game = factory.game_at_index(index).await?;
            // ZERO is the starting anchor, where the challenger scans from 0.
            if anchor_game != Address::ZERO && game.proxy == anchor_game {
                info!(anchor_game = %anchor_game, factory_index = index, "reached the anchor");
                break;
            }
            if game.game_type != config.game_type {
                continue;
            }
            if verifier.status(game.proxy).await? != GameStatus::InProgress {
                continue;
            }
            // A TEE-only, uncountered game is the challenger's Path 1. Games
            // that already carry a ZK proof or a counter are mid-dispute and
            // would confuse the assertions below.
            if verifier.tee_prover(game.proxy).await? == Address::ZERO {
                continue;
            }
            if verifier.zk_prover(game.proxy).await? != Address::ZERO {
                continue;
            }
            if verifier.countered_index(game.proxy).await? != 0 {
                continue;
            }
            // A clone from before a verification-key rotation cannot verify a
            // proof the current prover produces, so proving against it would
            // burn a SNARK and revert at submission.
            let game_hash = verifier.zk_aggregate_hash(game.proxy).await?;
            if game_hash != expected_hash {
                stale += 1;
                debug!(
                    game = %game.proxy,
                    factory_index = index,
                    game_hash = %game_hash,
                    expected_hash = %expected_hash,
                    "skipping game pinned to a superseded aggregation program"
                );
                continue;
            }
            let root_count = verifier.intermediate_output_roots(game.proxy).await?.len();
            let Ok(root_count @ 1..) = u64::try_from(root_count) else {
                continue;
            };

            info!(
                phase = %Phase::Setup,
                game = %game.proxy,
                factory_index = index,
                root_count,
                // A or B, so a log line says which game it is talking about
                // without cross-referencing the address.
                slot = if selected.is_empty() { "a" } else { "b" },
                "selected game"
            );
            selected.push(Candidate { address: game.proxy, root_count });
            if selected.len() == 2 {
                break;
            }
        }

        if let [game_a, game_b] = selected.as_slice() {
            return Ok((*game_a, *game_b));
        }
        ensure!(
            stale == 0,
            "found {} candidate game(s) pinned to an aggregation program other than the \
             implementation's {expected_hash}, and only {} usable; the factory's implementation \
             has changed and the proposer has not yet created enough games against it. Proving \
             against the older clones would spend a SNARK and then revert InvalidProof(), so this \
             run stops here instead",
            stale,
            selected.len()
        );
        bail!(
            "need two in-progress, uncountered games of type {} above the anchor in the newest \
             {} factory indices, found {}; the fork source may be behind, the proposer may be \
             stalled, or the anchor may have advanced past them",
            config.game_type,
            game_count - floor,
            selected.len()
        )
    }

    /// Attaches a real SNARK of B's canonical roots via `verifyProposalProof`.
    ///
    /// Signed by A. `zkProver` is set and `counteredIndex` stays 0, which is
    /// Path 4's dual-proof shape — not a challenge.
    async fn stage_dual_proof(
        config: &Config,
        fork_url: &Url,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        driver: &PrivateKeySigner,
        game: Candidate,
    ) -> Result<()> {
        let fork_config = Self::fork_config(config, fork_url, driver, game);
        let checkpoint = Checkpoint::proposal(&fork_config, verifier)
            .await
            .context("failed to build a canonical-range checkpoint for the dual-proof game")?;
        let l1_head = verifier.l1_head(game.address).await?;
        let game_l2_block_number = verifier.game_info(game.address).await?.l2_block_number;

        info!(
            game = %game.address,
            start_block = checkpoint.start_block,
            target_block = checkpoint.target_block(),
            interval = checkpoint.interval,
            "requesting SNARK of canonical roots to stage Path 4"
        );
        let proof_bytes = checkpoint
            .request_proof(&fork_config, driver.address(), l1_head, game_l2_block_number)
            .await
            .context("failed to request a SNARK of the dual-proof game's canonical roots")?;

        let tx_manager = Self::driver_tx_manager(provider, driver)
            .await
            .context("failed to build a tx manager for verifyProposalProof")?;
        let receipt = AggregateProofSubmitter::new(&tx_manager)
            .verify_proposal_proof(game.address, proof_bytes)
            .await
            .map_err(Self::name_revert)
            .context(
                "failed to submit verifyProposalProof; the prover-service produced a proof this \
                 game's ZK verifier rejected",
            )?;

        let zk_prover = verifier.zk_prover(game.address).await?;
        let countered_index = verifier.countered_index(game.address).await?;
        ensure!(
            zk_prover != Address::ZERO,
            "verifyProposalProof confirmed ({}) but zkProver is still zero",
            receipt.transaction_hash
        );
        ensure!(
            countered_index == 0,
            "verifyProposalProof set counteredIndex to {countered_index}; expected 0 \
             (a challenge, not a dual-proof proposal)"
        );

        info!(
            phase = %Phase::Path4,
            game = %game.address,
            tx_hash = %receipt.transaction_hash,
            zk_prover = %zk_prover,
            "staged Path 4 dual-proof game; counteredIndex is 0"
        );
        Ok(())
    }

    /// A TEE-typed dispute proof in the 66-byte `proofType(0) + signature(65)`
    /// shape the game slices before handing the signature to its TEE verifier.
    ///
    /// The signature is nonsense; the verifier is mocked out for the one call
    /// that consumes it, so only the length has to be right.
    fn dummy_tee_proof() -> Bytes {
        let mut proof = vec![0xab; 66];
        proof[0] = 0; // ProofType.TEE
        proof[65] = 27; // ECDSA v
        Bytes::from(proof)
    }

    /// Builds a tx manager for the driver key's setup transactions.
    async fn driver_tx_manager(
        provider: &RootProvider,
        driver: &PrivateKeySigner,
    ) -> Result<SimpleTxManager<RootProvider>> {
        let chain_id = provider.get_chain_id().await?;
        SimpleTxManager::new(
            provider.clone(),
            SignerConfig::local(driver.clone()),
            // Anvil mines on send: the default 10 confirmations never arrive
            // and the default 12s receipt poll is 12s of nothing. Send has no
            // default timeout at all, and these calls are not inside a
            // `poll_until`. Every other default is unreachable here.
            TxManagerConfig {
                num_confirmations: 1,
                receipt_query_interval: Duration::from_secs(1),
                tx_send_timeout: Duration::from_secs(180),
                ..Default::default()
            },
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .map_err(Into::into)
    }

    /// Stages Path 3: an invalid, ZK-only proposal.
    ///
    /// The game already carries a real SNARK of its canonical roots from
    /// [`Self::stage_dual_proof`]. Dropping its TEE proof and then corrupting a
    /// root leaves the `(teeProver == 0, zkProver != 0, counteredIndex == 0)`
    /// shape, with a ZK proposal that is now wrong, which the challenger
    /// classifies as `InvalidZkProposal`.
    ///
    /// The TEE proof is dropped through the game's own `nullify`, not by
    /// writing storage, so the game reaches the exact state a real TEE
    /// nullification produces — `proofCount` and `expectedResolution` included,
    /// and the TEE verifier's global `nullified` flag, which the mock sets
    /// through the same call a real one would. Only the signature check is
    /// mocked, for that one transaction: the driver key has no enclave to sign
    /// with. In `real` prover mode everything the challenger then does runs
    /// against the real, restored verifier.
    ///
    /// The challenger classifies every game from its proofs at the start of a
    /// scan, but reads its roots only when it reaches the game, a minute or more
    /// later on a busy factory. A scan that classified the game while it still
    /// had both proofs and reads its roots after the patch disputes it as
    /// `InvalidDualProposal` — Path 4 — and clears the ZK proof without Path 3
    /// ever being reached. No ordering of two writes closes that on its own, so
    /// the TEE proof goes first, while every root is still canonical, and the
    /// root is patched only once every scan that could have seen both proofs
    /// has finished ([`Self::await_scans_after`]). Until then the game is a
    /// valid ZK-only proposal, which the challenger leaves alone.
    ///
    /// `nullify` only requires the proven root to differ from the stored one
    /// (`_checkIntermediateRoot`), and the verifier is mocked for that call, so
    /// it proves [`PATH3_STAGING_ROOT`] at index 0 of the still-valid game.
    ///
    /// Returns the challenger's nonce, sampled before the fork is touched, the
    /// `invalid_dual_proposal_detected_total` reading from before staging
    /// began, which [`Self::await_path3`] requires to be unchanged, and the
    /// checkpoint that was corrupted.
    async fn stage_path3(
        config: &Config,
        fork_url: &Url,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        driver: &PrivateKeySigner,
        challenger: &PrivateKeySigner,
        game: Candidate,
    ) -> Result<(u64, f64, Checkpoint)> {
        let fork_config = Self::fork_config(config, fork_url, driver, game);
        // Sampled before anything is staged, for the reason given in `run_path1`.
        let nonce = provider.get_transaction_count(challenger.address()).await?;
        let dual_detected = Self::dual_proposals_detected(config).await?;

        let tee_verifier = verifier
            .tee_verifier_address(game.address)
            .await
            .context("failed to read the game's TEE verifier")?;
        let calldata = encode_nullify_calldata(Self::dummy_tee_proof(), 0, PATH3_STAGING_ROOT);
        let tx_manager = Self::driver_tx_manager(provider, driver)
            .await
            .context("failed to build a tx manager for the Path 3 TEE nullify")?;

        info!(
            game = %game.address,
            tee_verifier = %tee_verifier,
            "dropping the valid dual-proof game's TEE proof to stage Path 3"
        );
        let receipt = Self::with_mock_verifier(
            provider,
            tee_verifier,
            config.anchor_state_registry_addr,
            async {
                tx_manager
                    .send(TxCandidate {
                        tx_data: calldata,
                        to: Some(game.address),
                        ..Default::default()
                    })
                    .await
                    .map_err(Self::name_revert)
                    .context("failed to submit the Path 3 TEE nullify")
            },
        )
        .await?;
        ensure!(
            receipt.inner.status(),
            "the Path 3 TEE nullify reverted ({}); the game may check the TEE proof itself \
             rather than delegating to TEE_VERIFIER",
            receipt.transaction_hash
        );

        let state = Self::read_game_state(verifier, game.address).await?;
        ensure!(
            state.tee_prover == Address::ZERO,
            "the Path 3 TEE nullify confirmed ({}) but teeProver is still {}",
            receipt.transaction_hash,
            state.tee_prover
        );
        ensure!(
            state.zk_prover != Address::ZERO,
            "the Path 3 TEE nullify cleared the ZK proposal as well; there is nothing left to \
             dispute"
        );
        ensure!(
            state.countered_index == 0,
            "the Path 3 TEE nullify set counteredIndex to {}; expected 0 (an invalid ZK \
             proposal, not a challenge)",
            state.countered_index
        );

        // A real TEE nullification also nullifies the TEE verifier globally. The
        // mock reached that through the game's own `nullify` call and its real
        // registry guard, and the flag is storage, so it survives the restore.
        // Checked rather than assumed: a live TEE verifier would let other games
        // on the fork go on verifying TEE proofs, the one way this staged state
        // would differ from a genuine TEE-first Path 4.
        ensure!(
            verifier
                .verifier_nullified(tee_verifier)
                .await
                .context("failed to read the TEE verifier's nullified flag")?,
            "the TEE verifier at {tee_verifier} is still live after the Path 3 TEE nullify; the \
             fork would let other games verify TEE proofs that a real TEE-first Path 4 would have \
             blocked"
        );

        Self::await_scans_after(config, "the TEE proof was dropped").await?;

        // Only now does the game become invalid, and every scan from here on
        // classifies it from its ZK-only shape.
        let checkpoint = Checkpoint::patch(&fork_config, verifier)
            .await
            .context("failed to corrupt the ZK-only game on the fork")?;

        info!(
            phase = %Phase::Path3,
            game = %game.address,
            tx_hash = %receipt.transaction_hash,
            zk_prover = %state.zk_prover,
            invalid_index = checkpoint.index,
            "staged Path 3: an invalid ZK-only proposal"
        );
        Ok((nonce, dual_detected, checkpoint))
    }

    /// Runs `operation` with `verifier`'s code replaced by the mock verifier,
    /// restoring it either way.
    ///
    /// The restore is asserted, not assumed: leaving a permissive verifier on
    /// the fork in `real` mode would let every later assertion pass against a
    /// contract that verifies nothing. In `mock` mode the code restored is the
    /// mock itself, which is installed for the whole run.
    async fn with_mock_verifier<T>(
        provider: &RootProvider,
        verifier: Address,
        registry: Address,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let original = mock_verifier::install(provider, verifier, registry)
            .await
            .with_context(|| format!("failed to mock the verifier at {verifier}"))?;

        let outcome = operation.await;

        let restored = mock_verifier::restore(provider, &original).await.with_context(|| {
            format!("failed to restore the verifier at {verifier}; the fork is now unsound")
        });

        match (outcome, restored) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) | (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore_error)) => Err(error.wrap_err(format!("{restore_error:#}"))),
        }
    }

    /// Replaces both verifiers of every game under test with the mock, for the
    /// rest of the run.
    ///
    /// Games of one implementation share their verifiers, so this is normally
    /// two addresses; collecting them per game keeps it correct if A and B come
    /// from different implementations. Bystanders on the same verifiers see the
    /// mock too, which is harmless: nothing on the fork submits proofs for them.
    async fn install_mock_verifiers(
        config: &Config,
        provider: &RootProvider,
        verifier: &AggregateVerifierContractClient,
        games: [Address; 2],
    ) -> Result<()> {
        let mut verifiers = BTreeSet::new();
        for game in games {
            verifiers.insert(verifier.tee_verifier_address(game).await?);
            verifiers.insert(verifier.zk_verifier_address(game).await?);
        }
        for address in &verifiers {
            mock_verifier::install(provider, *address, config.anchor_state_registry_addr)
                .await
                .with_context(|| format!("failed to install the mock verifier at {address}"))?;
        }
        info!(
            phase = %Phase::Setup,
            verifiers = ?verifiers,
            "installed mock verifiers; proofs are no longer checked on this fork"
        );
        Ok(())
    }

    /// What the verifier used to check, checked here instead.
    ///
    /// A dispute carries the root its proof claims is correct at an index. The
    /// game only checks that root against its *own* stored one (it must differ,
    /// or for a challenged index match); that it is the *canonical* root was
    /// the proof's job. Under the mock verifier nothing checks it, so a
    /// challenger that proved the wrong root, or the wrong checkpoint, would
    /// still land its dispute. Every successful dispute the challenger sent to
    /// `game` must name the corrupted index and its canonical root.
    ///
    /// Read from the challenger's mined transactions, so it holds in `real`
    /// mode too, where it costs nothing and confirms what the proof proved.
    async fn assert_disputes_prove_canonical(
        config: &Config,
        provider: &RootProvider,
        challenger: &PrivateKeySigner,
        game: Address,
        checkpoint: &Checkpoint,
        phase: Phase,
    ) -> Result<()> {
        let latest = provider.get_block_number().await?;
        let mut disputes = 0usize;
        for number in config.fork_block + 1..=latest {
            let Some(block) =
                provider.get_block_by_number(BlockNumberOrTag::Number(number)).full().await?
            else {
                continue;
            };
            for tx in block.transactions.into_transactions() {
                if tx.from() != challenger.address() || tx.to() != Some(game) {
                    continue;
                }
                let Some(call) = decode_dispute_calldata(tx.input()) else {
                    continue;
                };
                let receipt = provider
                    .get_transaction_receipt(tx.tx_hash())
                    .await?
                    .ok_or_else(|| eyre!("no receipt for mined transaction {}", tx.tx_hash()))?;
                // A reverted dispute changed nothing, so it proved nothing either.
                if !receipt.status() {
                    continue;
                }
                ensure!(
                    call.intermediate_root_index == U256::from(checkpoint.index)
                        && call.intermediate_root_to_prove == checkpoint.expected_root,
                    "the challenger's {:?} {} on game {game} proved root {} at index {}, but the \
                     corrupted checkpoint is index {} with canonical root {}",
                    call.kind,
                    tx.tx_hash(),
                    call.intermediate_root_to_prove,
                    call.intermediate_root_index,
                    checkpoint.index,
                    checkpoint.expected_root
                );
                disputes += 1;
            }
        }
        ensure!(
            disputes > 0,
            "game {game} was disputed, but no successful nullify or challenge from the challenger \
             to it was found after fork block {}",
            config.fork_block
        );
        info!(
            phase = %phase,
            game = %game,
            disputes,
            index = checkpoint.index,
            root = %checkpoint.expected_root,
            "every dispute proved the canonical root at the corrupted index"
        );
        Ok(())
    }

    /// Path 3: the challenger must clear the invalid ZK-only proposal.
    ///
    /// Waiting for the ZK proof to go is unconditional — the game is invalid,
    /// and an E2E that returns `Ok` over an undisputed invalid game is worse
    /// than none. A timeout here fails the run.
    ///
    /// The route is then asserted, not inferred from the end state.
    /// [`Self::stage_path3`] never lets a scan see an invalid game with both
    /// proofs, so any `InvalidDualProposal` classification since
    /// `dual_before` fails the run;
    /// the challenger must have reached the game through `InvalidZkProposal`.
    /// The counter is judged here rather than at staging time because the
    /// driver increments it only after awaiting `validate_game`, so by the end
    /// of the dispute cycle any scan of the game has long since counted.
    ///
    /// `submitted_before` is the dispute-submission count from before the game
    /// was patched. Exactly one dispute clears Path 3, so anything above that
    /// went somewhere this scenario never corrupted — and a dispute that
    /// reverts moves no game state, so the per-game assertions cannot see it.
    #[expect(clippy::too_many_arguments, reason = "assertion inputs, all distinct")]
    async fn await_path3(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        challenger: &PrivateKeySigner,
        game: Candidate,
        nonce: u64,
        submitted_before: f64,
        dual_before: f64,
    ) -> Result<()> {
        let state = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to nullify the invalid ZK-only proposal",
            || async {
                let state = Self::read_game_state(verifier, game.address).await?;
                Ok((state.zk_prover == Address::ZERO).then_some(state))
            },
        )
        .await?;

        let dual_after = Self::dual_proposals_detected(config).await?;
        ensure!(
            dual_after <= dual_before,
            "the challenger classified {} game(s) as InvalidDualProposal during Path 3, but \
             staging waits out every scan that saw both proofs before it patches the root; game \
             {} may have been cleared as Path 4 rather than as an invalid ZK proposal",
            dual_after - dual_before,
            game.address
        );

        ensure!(
            state.tee_prover == Address::ZERO,
            "game {} grew a TEE proof during Path 3; only the challenger was acting on the fork",
            game.address
        );
        ensure!(
            state.countered_index == 0,
            "the challenger challenged game {} at index {} instead of nullifying its invalid ZK \
             proposal",
            game.address,
            state.countered_index.saturating_sub(1)
        );
        Self::assert_challenger_acted(
            provider,
            challenger,
            nonce,
            "Path 3 invalid ZK proposal nullified",
        )
        .await?;

        // Positive confirmation of the path, not just of the end state. Every
        // assertion above is satisfied by *any* route to a cleared ZK proof; this
        // is the one that says the challenger got there through
        // `InvalidZkProposal`.
        let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
        let classified = scrape.sum("base_challenger_invalid_zk_proposal_detected_total");
        ensure!(
            classified >= 1.0,
            "game {} was nullified but the challenger never classified an InvalidZkProposal; \
             Path 3 was cleared through some other path",
            game.address
        );

        let submitted = Self::disputes_submitted(config).await? - submitted_before;
        ensure!(
            submitted <= 1.0,
            "the challenger submitted {submitted} disputes to clear Path 3, which takes one; the \
             extra ones went to a game this scenario never corrupted, and a dispute that reverts \
             leaves its game state untouched"
        );

        info!(
            phase = %Phase::Path3,
            verdict = %Verdict::Pass,
            game = %game.address,
            disputes = submitted,
            "Path 3: invalid ZK proposal nullified"
        );
        Ok(())
    }

    /// Names the Solidity error behind a reverted dispute-game transaction.
    ///
    /// The tx manager reports a revert as a bare selector, which is unreadable
    /// in a log without the ABI to hand: a zeronet run died on `0x09bde339`
    /// and naming it `InvalidProof()` took a manual keccak sweep over the
    /// contract's 41 error signatures. Errors carrying no revert data pass
    /// through unchanged.
    fn name_revert<E>(error: E) -> eyre::Report
    where
        E: RevertData + std::error::Error + Send + Sync + 'static,
    {
        error.revert_data().map_or_else(
            || eyre!("{error}"),
            |data| eyre!("{error}").wrap_err(format!("reverted with {}", describe_revert(&data))),
        )
    }

    /// Waits until no scan that began before now can still act on the fork.
    ///
    /// A challenger step classifies every game in one pass, advances
    /// `games_scanned_total` once when that pass ends, and then validates and
    /// disputes from that classification. The first advance after now may
    /// belong to a pass that read the fork before `after`. The second belongs
    /// to a pass that began only once that step had finished processing, so
    /// every later step classifies from the fork as it is now.
    async fn await_scans_after(config: &Config, after: &str) -> Result<()> {
        let mut seen = Self::games_scanned(config).await?;
        for _ in 0..2 {
            let previous = seen;
            seen = Self::poll_until(
                config,
                config.dispute_timeout,
                &format!("the challenger to finish a scan that began after {after}"),
                || async move {
                    let now = Self::games_scanned(config).await?;
                    Ok((now > previous).then_some(now))
                },
            )
            .await?;
        }
        info!(after, "every challenger scan since started after this point");
        Ok(())
    }

    async fn games_scanned(config: &Config) -> Result<f64> {
        let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
        Ok(scrape.sum("base_challenger_games_scanned_total"))
    }

    /// Times the challenger has classified a game as `InvalidDualProposal`.
    ///
    /// Path 3 staging exposes that shape for one Anvil write plus one
    /// transaction, and this is what makes the window observable.
    async fn dual_proposals_detected(config: &Config) -> Result<f64> {
        let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
        Ok(scrape.sum("base_challenger_invalid_dual_proposal_detected_total"))
    }

    /// Total dispute transactions the challenger has submitted, reverted or not.
    ///
    /// Counted at submission rather than from game state: a dispute that
    /// reverts moves nothing on-chain, so it is invisible to every other
    /// assertion in this test.
    async fn disputes_submitted(config: &Config) -> Result<f64> {
        let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
        Ok(scrape.sum("base_challenger_nullify_tx_submitted_total")
            + scrape.sum("base_challenger_challenge_tx_submitted_total"))
    }

    /// Hands the fork and a funded key to the challenger sidecar, which is
    /// blocked on this file appearing.
    ///
    /// Written via a rename so the sidecar can never source a partial file.
    fn release_challenger(
        path: &Path,
        fork_url: &Url,
        zk_rpc_url: &Url,
        signer: &PrivateKeySigner,
    ) -> Result<()> {
        // Sourced after /envmapper/mapping.env, so these override the
        // config-service values for the run.
        //
        // The unsets are load-bearing. `--private-key` and `--signer-endpoint`
        // are `conflicts_with` in the shared signer CLI, and clap counts an
        // env-sourced value as present, so a mapping that carries the
        // production sidecar variables makes the challenger refuse to start.
        let contents = format!(
            "unset BASE_CHALLENGER_SIGNER_ENDPOINT\n\
             unset BASE_CHALLENGER_SIGNER_ADDRESS\n\
             export BASE_CHALLENGER_L1_ETH_RPC={fork_url}\n\
             export BASE_CHALLENGER_ZK_RPC_URL={zk_rpc_url}\n\
             export BASE_CHALLENGER_PRIVATE_KEY={}\n",
            hex::encode_prefixed(signer.to_bytes())
        );

        let staging = path.with_extension("tmp");
        std::fs::write(&staging, contents)
            .with_context(|| format!("failed to write {}", staging.display()))?;
        std::fs::rename(&staging, path)
            .with_context(|| format!("failed to publish {}", path.display()))?;

        info!(
            challenger_address = %signer.address(),
            env_file = %path.display(),
            "released the challenger onto the fork"
        );
        Ok(())
    }

    /// Waits until the challenger is up and has completed a scan.
    async fn await_first_scan(config: &Config) -> Result<()> {
        Self::poll_until(
            config,
            config.startup_timeout,
            "the challenger to complete a scan",
            || async {
                let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
                Ok((scrape.sum("base_challenger_up") >= 1.0
                    && scrape.sum("base_challenger_games_scanned_total") > 0.0)
                    .then_some(()))
            },
        )
        .await
    }

    /// Positive case: a challenger that disputes valid games fails here.
    ///
    /// The dual-proof game is still valid at this point (canonical roots,
    /// `counteredIndex == 0`) and must be left alone.
    async fn assert_quiet_on_valid_games(config: &Config) -> Result<()> {
        let before = Scrape::fetch(&config.challenger_metrics_url).await?;

        // Cumulative, not a delta. `games_scanned_total` is incremented for the
        // whole scanned range before any candidate is validated, so a scan that
        // has already completed can have disputed something before this
        // baseline was taken; a delta comparison would absorb it into `before`
        // and pass.
        for metric in DISPUTE_COUNTERS {
            let total = before.sum(metric);
            ensure!(
                total == 0.0,
                "{metric} is already {total} on the first completed scan; the challenger \
                 disputed something before the observation window opened, or the fork source \
                 carries a genuinely invalid game"
            );
        }

        info!(window = ?config.quiet_window, "observing the challenger against an unmodified fork");
        tokio::time::sleep(config.quiet_window).await;
        let after = Scrape::fetch(&config.challenger_metrics_url).await?;

        // `games_scanned_total` counts attempted factory indices and is
        // incremented even when every game query fails, so it cannot show that
        // any game was actually looked at. The validation histogram is closer
        // but still counts attempts, because its latency is recorded from a
        // drop guard that fires on the error path too. Subtracting the error
        // counter leaves the validations that actually computed a root.
        let attempted = after.sum(VALIDATIONS) - before.sum(VALIDATIONS);
        let failed = after.sum(VALIDATION_ERRORS) - before.sum(VALIDATION_ERRORS);
        let validated = attempted - failed;
        let scanned = after.sum("base_challenger_games_scanned_total")
            - before.sum("base_challenger_games_scanned_total");
        ensure!(
            validated > 0.0,
            "the challenger completed no validation in {:?} ({attempted} attempted, {failed} \
             failed, {scanned} indices scanned); a quiet window over games it never managed to \
             check proves nothing",
            config.quiet_window
        );

        for metric in DISPUTE_COUNTERS {
            let delta = after.sum(metric) - before.sum(metric);
            ensure!(
                delta == 0.0,
                "{metric} advanced by {delta} while every game on the fork was valid"
            );
        }

        // Some failures are tolerable — they are usually the L2 RPC rather than
        // the challenger — so they are reported rather than fatal. A storm that
        // swallows every game fails the assertion above instead.
        if failed > 0.0 {
            warn!(validation_errors = failed, "the challenger reported validation errors");
        }

        info!(
            phase = %Phase::QuietWindow,
            verdict = %Verdict::Pass,
            games_validated = validated,
            indices_scanned = scanned,
            "the challenger left every valid game alone"
        );
        Ok(())
    }

    /// Path 1: patch game A and wait for a TEE nullify or ZK challenge.
    async fn run_path1(
        config: &Config,
        fork_url: &Url,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        driver: &PrivateKeySigner,
        challenger: &PrivateKeySigner,
        game: Candidate,
    ) -> Result<(Path1Outcome, Checkpoint)> {
        let fork_config = Self::fork_config(config, fork_url, driver, game);
        // Before the patch, not after. `Checkpoint::patch` mutates the game and
        // then reads it back several times; a challenger that scans during
        // those readbacks would have already spent the nonce this baseline is
        // meant to precede, and its correct dispute would read as nobody's.
        let nonce = provider.get_transaction_count(challenger.address()).await?;
        let checkpoint = Checkpoint::patch(&fork_config, verifier)
            .await
            .context("failed to corrupt an intermediate output root on the fork")?;
        info!(
            game = %game.address,
            invalid_index = checkpoint.index,
            start_block = checkpoint.start_block,
            target_block = checkpoint.target_block(),
            "corrupted intermediate output root; waiting for the challenger to dispute"
        );

        let outcome = Self::await_dispute(
            config,
            verifier,
            provider,
            game.address,
            challenger,
            nonce,
            checkpoint.index + 1,
        )
        .await?;
        Self::assert_disputes_prove_canonical(
            config,
            provider,
            challenger,
            game.address,
            &checkpoint,
            Phase::Path1,
        )
        .await?;
        Ok((outcome, checkpoint))
    }

    /// The challenger must leave game A alone once it has acted on it.
    ///
    /// After a ZK challenge this is Path 2 skip: a legitimate challenge of a
    /// wrong TEE root must stand, and a challenger that "defends" it fails
    /// here. After a TEE nullify there is no challenge to leave standing, and
    /// the same window instead proves the challenger does not re-dispute a game
    /// it has already nullified. Both branches are checked, so idempotence is
    /// covered on every run rather than only on the ZK half.
    ///
    /// The nonce is watched alongside the state because a dispute that reverts
    /// moves none of the three fields: a challenger stuck re-challenging a
    /// legitimate challenge, or re-nullifying an already-nullified game, is
    /// invisible to the state comparison alone.
    ///
    /// The `path1-path2` scenario restores the canonical root after this
    /// window, turning the recorded challenge into Path 2's fraudulent case.
    async fn assert_game_a_settled(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        challenger: &PrivateKeySigner,
        game: Candidate,
        path1: Path1Outcome,
    ) -> Result<()> {
        let claim = match path1 {
            Path1Outcome::ZkChallenge => "Path 2 skip: a legitimate ZK challenge must stand",
            Path1Outcome::TeeNullify => {
                "idempotence: an already-nullified game must not be disputed again"
            }
        };

        let before = Self::read_game_state(verifier, game.address).await?;
        // Nothing on the fork is disputable for the length of this window —
        // game B is still valid and the bystanders always were — so the
        // challenger has no reason to send anything at all. A fee-bumped
        // replacement reuses its nonce, so only a genuinely new transaction
        // moves this.
        let nonce_before = provider.get_transaction_count(challenger.address()).await?;
        info!(
            game = %game.address,
            window = ?config.quiet_window,
            state = ?before,
            claim,
            "observing the settle window"
        );
        tokio::time::sleep(config.quiet_window).await;

        let after = Self::read_game_state(verifier, game.address).await?;
        let nonce_after = provider.get_transaction_count(challenger.address()).await?;
        ensure!(
            after == before,
            "{claim} — game {} moved from {before:?} to {after:?}",
            game.address
        );
        ensure!(
            nonce_after == nonce_before,
            "{claim} — game {} is unchanged, but the challenger sent {} transaction(s) during the \
             settle window; a dispute that reverts leaves the game state untouched",
            game.address,
            nonce_after - nonce_before
        );

        // The settle window carries two different claims depending on how Path 1
        // landed, so the phase follows the claim rather than the call site.
        let phase = match path1 {
            Path1Outcome::ZkChallenge => Phase::Path2Skip,
            Path1Outcome::TeeNullify => Phase::Path1,
        };
        info!(
            phase = %phase,
            verdict = %Verdict::Pass,
            game = %game.address,
            claim,
            "the settle claim held"
        );
        Ok(())
    }

    /// Path 2 dispute: restore the correct TEE root underneath the recorded
    /// Path 1 challenge, then require the challenger to nullify that challenge.
    async fn run_path2(
        config: &Config,
        fork_config: ForkConfig,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        challenger: &PrivateKeySigner,
        game: Candidate,
        checkpoint: Checkpoint,
    ) -> Result<()> {
        let nonce = provider.get_transaction_count(challenger.address()).await?;
        checkpoint
            .restore(&fork_config, verifier)
            .await
            .context("failed to restore the canonical root for Path 2")?;
        info!(
            game = %game.address,
            challenged_index = checkpoint.index,
            "restored the correct root; waiting for Path 2"
        );

        let state = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to nullify the fraudulent ZK challenge",
            || async {
                let state = Self::read_game_state(verifier, game.address).await?;
                Ok((state.zk_prover == Address::ZERO && state.countered_index == 0)
                    .then_some(state))
            },
        )
        .await?;
        ensure!(
            state.tee_prover != Address::ZERO,
            "Path 2 cleared the TEE proof instead of the fraudulent ZK challenge"
        );
        Self::assert_challenger_acted(
            provider,
            challenger,
            nonce,
            "Path 2 fraudulent challenge nullified",
        )
        .await?;
        // Covers Path 1's challenge as well as this nullify: both name the same
        // index, and in both the canonical root is the one to prove.
        Self::assert_disputes_prove_canonical(
            config,
            provider,
            challenger,
            game.address,
            &checkpoint,
            Phase::Path2Dispute,
        )
        .await?;
        info!(
            phase = %Phase::Path2Dispute,
            verdict = %Verdict::Pass,
            game = %game.address,
            "Path 2: fraudulent ZK challenge nullified"
        );
        Ok(())
    }

    /// Reads everything the challenger is able to change about a game.
    async fn read_game_state(
        verifier: &AggregateVerifierContractClient,
        game: Address,
    ) -> Result<GameState> {
        Ok(GameState {
            tee_prover: verifier.tee_prover(game).await?,
            zk_prover: verifier.zk_prover(game).await?,
            countered_index: verifier.countered_index(game).await?,
        })
    }

    /// Records the prover state of every readable game in the lookback window
    /// apart from the two under test.
    ///
    /// Games whose prover fields do not read are skipped rather than fatal:
    /// they are a different verifier shape, so the challenger cannot move them
    /// through the fields this test watches.
    async fn snapshot_bystanders(
        config: &Config,
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
        under_test: [Address; 2],
    ) -> Result<Vec<(Address, GameState)>> {
        let game_count = factory.game_count().await?;
        let floor = game_count.saturating_sub(config.game_lookback);

        let mut snapshot = Vec::new();
        for index in floor..game_count {
            let game = factory.game_at_index(index).await?;
            if under_test.contains(&game.proxy) {
                continue;
            }
            if let Ok(state) = Self::read_game_state(verifier, game.proxy).await {
                snapshot.push((game.proxy, state));
            }
        }

        info!(
            phase = %Phase::Setup,
            bystanders = snapshot.len(),
            lookback = game_count - floor,
            "snapshotted games the challenger must not touch"
        );
        Ok(snapshot)
    }

    /// The challenger may only have moved the two games this test corrupted.
    ///
    /// Catches collateral damage the per-game assertions cannot see: a
    /// challenger misconfigured on `game_type`, one with a broken lookback, or
    /// one that starts disputing indiscriminately after its first dispute.
    ///
    /// The leniency in [`Self::snapshot_bystanders`] does not carry over here:
    /// it decides what to watch, this decides whether the run passes.
    async fn assert_bystanders_untouched(
        verifier: &AggregateVerifierContractClient,
        snapshot: &[(Address, GameState)],
    ) -> Result<()> {
        for (game, before) in snapshot {
            // Not skipped on a read failure, unlike the snapshot pass. Every
            // game in here already read cleanly once, so it is the shape this
            // check watches; a read that fails now is the RPC, and continuing
            // past it would drop a game from the only assertion that catches a
            // challenger disputing indiscriminately. Fail closed and say why.
            let after = Self::read_game_state(verifier, *game).await.with_context(|| {
                format!(
                    "failed to re-read bystander game {game}; it read cleanly when snapshotted, \
                     so the collateral-damage check could not be completed"
                )
            })?;
            ensure!(
                after == *before,
                "the challenger moved game {game}, which this test never corrupted: \
                 {before:?} -> {after:?}"
            );
        }

        info!(
            phase = %Phase::Bystanders,
            verdict = %Verdict::Pass,
            bystanders = snapshot.len(),
            "the challenger touched no game it was not given"
        );
        Ok(())
    }

    /// Path 4, plus Path 3 when the TEE proof is nullified first.
    ///
    /// A dual-proof game takes two disputes to clear, and the challenger is
    /// free to drop either proof first. If B is registered as a TEE proposer it
    /// nullifies the TEE proof, leaving a ZK-only game the next scan disputes as
    /// Path 3. B is normally a throwaway, unregistered key, so the TEE submission
    /// fails and the ZK fallback nullifies the global ZK verifier. The remaining
    /// TEE proof cannot then be challenged with another ZK proof on the same fork,
    /// so that branch ends after Path 4.
    ///
    /// Returns whether Path 3 was reached and asserted in situ.
    async fn run_path4(
        config: &Config,
        fork_url: &Url,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        driver: &PrivateKeySigner,
        challenger: &PrivateKeySigner,
        game: Candidate,
    ) -> Result<bool> {
        let fork_config = Self::fork_config(config, fork_url, driver, game);
        // Sampled before the patch for the reason given in `run_path1`.
        let nonce = provider.get_transaction_count(challenger.address()).await?;
        let checkpoint = Checkpoint::patch(&fork_config, verifier)
            .await
            .context("failed to corrupt the dual-proof game on the fork")?;
        info!(
            game = %game.address,
            invalid_index = checkpoint.index,
            start_block = checkpoint.start_block,
            target_block = checkpoint.target_block(),
            "corrupted dual-proof game; waiting for Path 4"
        );

        // Path 4 is done when either proof is gone; which one tells us what the
        // game has become, and so which path must clear the remainder. Both
        // fields are read in the same observation because the challenger can
        // outrun `poll_interval` and clear both before the first look — reading
        // only `teeProver` there would call that "TEE first" and then wait for a
        // ZK nullify that has already happened.
        let (tee_cleared, zk_cleared) = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to nullify one of the dual-proof game's two proofs",
            || async {
                let tee = verifier.tee_prover(game.address).await? == Address::ZERO;
                let zk = verifier.zk_prover(game.address).await? == Address::ZERO;
                Ok((tee || zk).then_some((tee, zk)))
            },
        )
        .await?;

        if tee_cleared && zk_cleared {
            info!(
                phase = %Phase::Path4,
                verdict = %Verdict::Pass,
                branch = "both-cleared",
                game = %game.address,
                "Path 4 and its follow-up both landed inside one poll"
            );
        } else if tee_cleared {
            info!(
                phase = %Phase::Path4,
                branch = "tee-first",
                game = %game.address,
                "Path 4: TEE proof nullified, ZK proof remains"
            );
            Self::poll_until(
                config,
                config.dispute_timeout,
                "the challenger to ZK-nullify the remaining proof",
                || async {
                    Ok((verifier.zk_prover(game.address).await? == Address::ZERO).then_some(()))
                },
            )
            .await?;
            info!(
                phase = %Phase::Path3,
                verdict = %Verdict::Pass,
                reached = "in-situ",
                game = %game.address,
                "Path 3: ZK proof nullified"
            );
        } else {
            info!(
                phase = %Phase::Path4,
                verdict = %Verdict::Pass,
                branch = "zk-fallback",
                game = %game.address,
                "Path 4: ZK fallback nullified, TEE proof remains"
            );
        }

        let expected_transactions = if tee_cleared { 2 } else { 1 };
        let nonce_after = provider.get_transaction_count(challenger.address()).await?;
        ensure!(
            nonce_after >= nonce + expected_transactions,
            "game {} changed but the challenger sent {} transaction(s), fewer than the expected \
             {expected_transactions}; something other than the challenger disputed it",
            game.address,
            nonce_after - nonce
        );
        Self::assert_disputes_prove_canonical(
            config,
            provider,
            challenger,
            game.address,
            &checkpoint,
            Phase::Path4,
        )
        .await?;
        info!(
            phase = %Phase::Path4,
            verdict = %Verdict::Pass,
            game = %game.address,
            transactions = nonce_after - nonce,
            "the challenger completed Path 4"
        );
        // Path 3 was reached and asserted in situ only on the TEE-first branch.
        Ok(tee_cleared && !zk_cleared)
    }

    /// Negative case: the challenger must dispute the corrupted game, and it
    /// must be the challenger that does it.
    ///
    /// Both dispute paths count. A corrupted TEE-only game is Path 1, which
    /// tries a TEE proof first and falls back to a ZK challenge; insisting on
    /// `nullify` would fail the run whenever the TEE prover is briefly down.
    async fn await_dispute(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        game: Address,
        challenger: &PrivateKeySigner,
        nonce_before: u64,
        expected_countered: u64,
    ) -> Result<Path1Outcome> {
        let outcome = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to dispute the corrupted game",
            || async {
                if verifier.tee_prover(game).await? == Address::ZERO {
                    return Ok(Some(Path1Outcome::TeeNullify));
                }
                let countered = verifier.countered_index(game).await? != 0;
                if countered && verifier.zk_prover(game).await? != Address::ZERO {
                    return Ok(Some(Path1Outcome::ZkChallenge));
                }
                Ok(None)
            },
        )
        .await?;

        // Outside the poll on purpose. `poll_until` swallows a predicate error
        // and retries, so an `ensure!` in there would surface as a timeout
        // rather than as the mismatch it is.
        if matches!(outcome, Path1Outcome::ZkChallenge) {
            let countered = verifier.countered_index(game).await?;
            let zk_prover = verifier.zk_prover(game).await?;
            ensure!(
                countered == expected_countered,
                "the challenger countered intermediate root {} of game {game}, but the root this \
                 run corrupted is {}; an accepted proof against a different checkpoint is not a \
                 dispute of the corruption",
                countered.saturating_sub(1),
                expected_countered.saturating_sub(1)
            );
            ensure!(
                zk_prover == challenger.address(),
                "game {game} was challenged by {zk_prover}, not by the challenger {}",
                challenger.address()
            );
        }

        let label = match outcome {
            Path1Outcome::TeeNullify => "nullified via TEE proof",
            Path1Outcome::ZkChallenge => "challenged via ZK proof",
        };
        // ponytail: a nonce bump plus the state change is enough to attribute
        // the dispute — A only signs setup, so a bump on B is still the
        // challenger. Walk the mined blocks for the calling address if this
        // ever needs to name the exact transaction.
        let nonce_after =
            Self::assert_challenger_acted(provider, challenger, nonce_before, label).await?;

        info!(
            phase = %Phase::Path1,
            verdict = %Verdict::Pass,
            game = %game,
            outcome = label,
            transactions = nonce_after - nonce_before,
            "the challenger disputed the corrupted game"
        );
        Ok(outcome)
    }

    async fn assert_challenger_acted(
        provider: &RootProvider,
        challenger: &PrivateKeySigner,
        nonce_before: u64,
        outcome: &str,
    ) -> Result<u64> {
        let nonce_after = provider.get_transaction_count(challenger.address()).await?;
        ensure!(
            nonce_after > nonce_before,
            "the game was {outcome} but the challenger's nonce is unchanged at {nonce_before}; \
             something other than the challenger disputed it"
        );
        Ok(nonce_after)
    }

    /// Appends the challenger's failure counters to a dispute timeout, which is
    /// otherwise indistinguishable from "nothing happened".
    async fn annotate_timeout(error: eyre::Report, config: &Config) -> eyre::Report {
        let Ok(scrape) = Scrape::fetch(&config.challenger_metrics_url).await else {
            return error;
        };
        error.wrap_err(format!(
            "challenger counters at timeout: invalid={} validation_errors={} \
             nullify_submitted={} nullify_reverted={} challenge_submitted={} \
             challenge_reverted={} pending_proofs={}",
            scrape.sum("base_challenger_games_invalid_total"),
            scrape.sum("base_challenger_validation_errors_total"),
            scrape.sum("base_challenger_nullify_tx_submitted_total"),
            scrape.label_sum("base_challenger_nullify_tx_outcome_total", "reverted"),
            scrape.sum("base_challenger_challenge_tx_submitted_total"),
            scrape.label_sum("base_challenger_challenge_tx_outcome_total", "reverted"),
            scrape.sum("base_challenger_pending_proofs"),
        ))
    }

    fn fork_config(
        config: &Config,
        fork_url: &Url,
        driver: &PrivateKeySigner,
        candidate: Candidate,
    ) -> ForkConfig {
        ForkConfig {
            l1_rpc_url: fork_url.clone(),
            l2_provider: L2HttpProvider::new_http(config.l2_eth_rpc.clone()),
            prover_service_url: config.zk_rpc_url.clone(),
            dispute_game_factory: config.dispute_game_factory_addr,
            game_address: candidate.address,
            game_type: config.game_type,
            private_key: driver.clone(),
            intent: None,
            zk_backend: ZkBackend::default(),
            // The last checkpoint covers the most recent L2 blocks, which are
            // the ones the L2 RPC is most likely to still serve.
            invalid_index: Some(candidate.root_count - 1),
            patch_invalid_game: true,
            poll_interval: config.poll_interval,
            poll_timeout: config.dispute_timeout,
        }
    }

    /// Polls `check` every `poll_interval` until it yields a value or `budget`
    /// elapses.
    async fn poll_until<T, F, Fut>(
        config: &Config,
        budget: Duration,
        waiting_for: &str,
        mut check: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        let mut last_error = None;
        match tokio::time::timeout(budget, async {
            loop {
                match check().await {
                    Ok(Some(value)) => return Ok(value),
                    Ok(None) => {}
                    // The fork and the challenger are both starting up; a read
                    // that fails now routinely succeeds on the next tick.
                    Err(error) => {
                        warn!(error = %error, "poll failed; retrying");
                        last_error = Some(error);
                    }
                }
                tokio::time::sleep(config.poll_interval).await;
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let msg = format!("timed out after {budget:?} waiting for {waiting_for}");
                let error = match last_error {
                    Some(error) => error.wrap_err(msg),
                    None => eyre!("{msg}"),
                };
                Err(Self::annotate_timeout(error, config).await)
            }
        }
    }
}

/// Exposes the raw revert bytes an error carries, if any.
///
///
/// Two unrelated error types reach the same logging path, and only the bytes
/// matter to [`ChallengerE2e::name_revert`].
pub trait RevertData {
    /// Returns the EVM revert data, or `None` if this error is not a revert.
    fn revert_data(&self) -> Option<Bytes>;
}

impl RevertData for TxManagerError {
    fn revert_data(&self) -> Option<Bytes> {
        match self {
            Self::ExecutionReverted { data, .. } => data.clone(),
            _ => None,
        }
    }
}

impl RevertData for ProofSubmissionError {
    fn revert_data(&self) -> Option<Bytes> {
        match self {
            Self::TxManager(error) => error.revert_data(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the helper: a zeronet run failed on a bare `0x09bde339`,
    /// which cost a manual keccak sweep to identify.
    #[test]
    fn name_revert_names_the_error_instead_of_printing_a_selector() {
        let reverted = TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from_static(&[0x09, 0xbd, 0xe3, 0x39])),
        };
        let report = format!("{:#}", ChallengerE2e::name_revert(reverted));
        assert!(report.contains("InvalidProof"), "{report}");

        // Through the submission wrapper too, which is the path that actually
        // failed on zeronet.
        let wrapped = ProofSubmissionError::TxManager(TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from_static(&[0x09, 0xbd, 0xe3, 0x39])),
        });
        let report = format!("{:#}", ChallengerE2e::name_revert(wrapped));
        assert!(report.contains("InvalidProof"), "{report}");
    }

    #[test]
    fn name_revert_passes_through_errors_that_are_not_reverts() {
        let report = format!("{:#}", ChallengerE2e::name_revert(TxManagerError::NonceTooLow));
        assert!(report.contains("nonce too low"), "{report}");
        assert!(!report.contains("reverted with"), "{report}");
    }

    /// These strings are what Datadog facets, dashboards and monitors filter
    /// on, so a rename is a breaking change to whatever watches them.
    #[test]
    fn phase_and_verdict_field_values_are_stable() {
        assert_eq!(Phase::Setup.as_str(), "setup");
        assert_eq!(Phase::QuietWindow.as_str(), "quiet-window");
        assert_eq!(Phase::Path1.as_str(), "path1");
        assert_eq!(Phase::Path2Skip.as_str(), "path2-skip");
        assert_eq!(Phase::Path2Dispute.as_str(), "path2-dispute");
        assert_eq!(Phase::Path3.as_str(), "path3");
        assert_eq!(Phase::Path4.as_str(), "path4");
        assert_eq!(Phase::Bystanders.as_str(), "bystanders");
        assert_eq!(Verdict::Pass.as_str(), "pass");
        assert_eq!(Verdict::Skip.as_str(), "skip");

        // `Display` is what the `%` sigil uses in the tracing macros, so it has
        // to agree with `as_str` or the logs and this test diverge.
        assert_eq!(Phase::Path2Dispute.to_string(), Phase::Path2Dispute.as_str());
        assert_eq!(Verdict::Skip.to_string(), Verdict::Skip.as_str());
    }

    #[test]
    fn private_key_env_is_0x_hex_and_round_trips() {
        let signer = PrivateKeySigner::random();
        let encoded = hex::encode_prefixed(signer.to_bytes());
        assert!(encoded.starts_with("0x"), "{encoded}");
        assert_eq!(encoded.len(), 66);
        assert!(encoded[2..].chars().all(|c| c.is_ascii_hexdigit()));
        let parsed: PrivateKeySigner = encoded.parse().expect("challenger clap parse");
        assert_eq!(parsed.address(), signer.address());
    }
}
