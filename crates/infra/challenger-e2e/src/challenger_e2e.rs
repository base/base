//! Drives a real challenger binary against a throwaway fork of the target L1.

use std::{path::Path, sync::Arc, time::Duration};

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, U256, hex};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
    GameStatus,
};
use base_proof_rpc::L2HttpProvider;
use base_proof_submission::AggregateProofSubmitter;
use base_prover_service_protocol::ZkBackend;
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use base_zk_fork_dispute::{Checkpoint, Config as ForkConfig};
use clap::Parser;
use eyre::{Context, Result, bail, ensure, eyre};
use tracing::{info, warn};
use url::Url;

use crate::{config::Config, metrics::Scrape};

/// Wei granted to each throwaway account on the fork. Orders of magnitude more
/// than a dispute costs, and worthless outside the pod.
const FUNDING_WEI: u128 = 100_000_000_000_000_000_000;

/// Handshake file the driver writes to release the challenger sidecar, which
/// blocks on it appearing. The sidecar hardcodes the same path, so this was
/// never independently configurable.
const CHALLENGER_ENV_FILE: &str = "/shared/challenger.env";

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

/// Behavioural end-to-end test of the challenger.
///
/// See the crate README for the full argument; the short version is that the
/// fork is built from a real chain and the games under test were created and
/// verified on that chain, so nothing about the dispute is stubbed.
#[derive(Debug)]
pub struct ChallengerE2e;

impl ChallengerE2e {
    /// Runs the test to completion. An `Ok` return means the challenger passed.
    pub async fn run() -> Result<()> {
        let config = Config::parse();

        // Two distinct accounts: A (driver) signs setup only, B is the
        // challenger. Both are generated per run and never leave the pod.
        let driver = PrivateKeySigner::random();
        let challenger = PrivateKeySigner::random();

        // Held until the end of run(); the fork dies with this binding.
        let anvil = Self::spawn_fork(&config)?;
        let fork_url = anvil.endpoint_url();
        let provider: RootProvider = RootProvider::new_http(fork_url.clone());
        Self::fund(&provider, &[driver.address(), challenger.address()]).await?;

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

        Self::stage_dual_proof(&config, &fork_url, &verifier, &provider, &driver, game_b).await?;

        Self::release_challenger(&fork_url, &challenger)?;
        Self::await_first_scan(&config).await?;

        Self::assert_quiet_on_valid_games(&config).await?;

        let path1 =
            Self::run_path1(&config, &fork_url, &verifier, &provider, &driver, &challenger, game_a)
                .await?;
        Self::assert_game_a_settled(&config, &verifier, &provider, &challenger, game_a, path1)
            .await?;
        Self::run_path4_then_3(
            &config,
            &fork_url,
            &verifier,
            &provider,
            &driver,
            &challenger,
            game_b,
        )
        .await?;

        Self::assert_bystanders_untouched(&verifier, &bystanders).await?;

        Ok(())
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
            let root_count = verifier.intermediate_output_roots(game.proxy).await?.len();
            let Ok(root_count @ 1..) = u64::try_from(root_count) else {
                continue;
            };

            info!(
                game = %game.proxy,
                factory_index = index,
                root_count,
                slot = selected.len(),
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

        let chain_id = provider.get_chain_id().await?;
        let tx_manager = SimpleTxManager::new(
            provider.clone(),
            SignerConfig::local(driver.clone()),
            // Anvil mines on send: the default 10 confirmations never arrive
            // and the default 12s receipt poll is 12s of nothing. Send has no
            // default timeout at all, and this call is not inside a
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
        .context("failed to build a tx manager for verifyProposalProof")?;
        let receipt = AggregateProofSubmitter::new(&tx_manager)
            .verify_proposal_proof(game.address, proof_bytes)
            .await
            .context("failed to submit verifyProposalProof")?;

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
            game = %game.address,
            tx_hash = %receipt.transaction_hash,
            zk_prover = %zk_prover,
            "staged Path 4 dual-proof game; counteredIndex is 0"
        );
        Ok(())
    }

    /// Hands the fork and a funded key to the challenger sidecar, which is
    /// blocked on this file appearing.
    ///
    /// Written via a rename so the sidecar can never source a partial file.
    fn release_challenger(fork_url: &Url, signer: &PrivateKeySigner) -> Result<()> {
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
             export BASE_CHALLENGER_PRIVATE_KEY={}\n",
            hex::encode_prefixed(signer.to_bytes())
        );

        let path = Path::new(CHALLENGER_ENV_FILE);
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
    ) -> Result<Path1Outcome> {
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

        Self::await_dispute(
            config,
            verifier,
            provider,
            game.address,
            challenger,
            nonce,
            checkpoint.index + 1,
        )
        .await
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
    /// Path 2 *dispute* (fraudulent ZK against a correct TEE root) is not
    /// staged: the real prover cannot produce a wrong-root proof the real
    /// verifier accepts.
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

        info!(game = %game.address, claim, "the settle claim held");
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

        info!(bystanders = snapshot.len(), "the challenger touched no game it was not given");
        Ok(())
    }

    /// Path 4 then whatever Path 4 leaves behind. Each step must move B's nonce.
    ///
    /// A dual-proof game takes two disputes to clear, and the challenger is
    /// free to drop either proof first. With a TEE prover available it nullifies
    /// the TEE proof, leaving a ZK-only game the next scan disputes as Path 3.
    /// When the TEE request or submission fails it falls back to a ZK nullify —
    /// a supported path, covered by the dual-proof ZK-fallback test in
    /// `crates/proof/challenge/tests/driver.rs` — which leaves a TEE-only game
    /// the next scan disputes as Path 1, by nullify or by challenge. Demanding
    /// the TEE proof go first would sit out the whole dispute budget on a
    /// challenger doing exactly what it is supposed to.
    async fn run_path4_then_3(
        config: &Config,
        fork_url: &Url,
        verifier: &AggregateVerifierContractClient,
        provider: &RootProvider,
        driver: &PrivateKeySigner,
        challenger: &PrivateKeySigner,
        game: Candidate,
    ) -> Result<()> {
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
            info!(game = %game.address, "Path 4 and its follow-up both landed inside one poll");
        } else if tee_cleared {
            info!(game = %game.address, "Path 4: TEE proof nullified, ZK proof remains");
            Self::poll_until(
                config,
                config.dispute_timeout,
                "the challenger to ZK-nullify the remaining proof",
                || async {
                    Ok((verifier.zk_prover(game.address).await? == Address::ZERO).then_some(()))
                },
            )
            .await?;
            info!(game = %game.address, "Path 3: ZK proof nullified");
        } else {
            info!(game = %game.address, "Path 4: ZK fallback nullified, TEE proof remains");
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
            info!(game = %game.address, ?outcome, "Path 1: the remaining TEE proof was disputed");
        }

        // Two disputes clear a dual-proof game, whichever order they arrived in.
        // Asserted once against the pre-patch baseline rather than per step: a
        // per-step delta attributes both transactions to the first step whenever
        // the challenger beats the poll, and then demands a third that is never
        // coming.
        let nonce_after = provider.get_transaction_count(challenger.address()).await?;
        ensure!(
            nonce_after >= nonce + 2,
            "both of game {}'s proofs are gone but the challenger sent {} transaction(s), not the \
             two a dual-proof game takes; something other than the challenger disputed it",
            game.address,
            nonce_after - nonce
        );
        info!(
            game = %game.address,
            transactions = nonce_after - nonce,
            "the challenger cleared both of the dual-proof game's proofs"
        );
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
