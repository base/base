//! Drives a real challenger binary against a throwaway fork of the target L1.

use std::{sync::Arc, time::Duration};

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, U256, hex};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameStatus,
};
use base_proof_rpc::L2HttpProvider;
use base_proof_submission::AggregateProofSubmitter;
use base_prover_service_protocol::ZkBackend;
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use base_zk_fork_dispute::{Checkpoint, Config as ForkConfig};
use clap::Parser;
use eyre::{Context, Result, bail, ensure};
use tracing::{info, warn};
use url::Url;

use crate::{config::Config, metrics::Scrape};

/// Wei granted to each throwaway account on the fork. Orders of magnitude more
/// than a dispute costs, and worthless outside the pod.
const FUNDING_WEI: u128 = 100_000_000_000_000_000_000;

/// A game the challenger has been observed to accept, plus its root count.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    address: Address,
    root_count: u64,
}

/// How Path 1 landed. Path 2 skip only applies to [`Self::ZkChallenge`].
#[derive(Debug, Clone, Copy)]
enum Path1Outcome {
    TeeNullify,
    ZkChallenge,
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

        let anvil = Self::spawn_fork(&config)?;
        let fork_url = anvil.endpoint_url();
        let provider: RootProvider = RootProvider::new_http(fork_url.clone());
        Self::fund(&provider, &[driver.address(), challenger.address()]).await?;

        let factory = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            fork_url.clone(),
        )
        .context("failed to build a DisputeGameFactory client for the fork")?;
        let verifier = AggregateVerifierContractClient::new(fork_url.clone())
            .context("failed to build an AggregateVerifier client for the fork")?;

        // Chosen before the challenger boots so the positive case below is
        // measured against a fork that already contains the target games.
        let (game_a, game_b) = Self::select_games(&config, &factory, &verifier).await?;

        Self::stage_dual_proof(&config, &fork_url, &verifier, &provider, &driver, game_b).await?;

        Self::release_challenger(&config, &fork_url, &challenger)?;
        Self::await_first_scan(&config).await?;

        Self::assert_quiet_on_valid_games(&config).await?;

        let path1 =
            Self::run_path1(&config, &fork_url, &verifier, &provider, &driver, &challenger, game_a)
                .await?;
        Self::run_path2_skip(&config, &verifier, game_a, path1).await?;
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

        // Anvil dies with the process anyway; the explicit drop documents that
        // nothing above may outlive the fork.
        drop(anvil);
        Ok(())
    }

    fn spawn_fork(config: &Config) -> Result<AnvilInstance> {
        info!(fork_source = %config.l1_eth_rpc, port = config.anvil_port, "spawning L1 fork");
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
    ) -> Result<(Candidate, Candidate)> {
        let game_count = factory.game_count().await?;
        if game_count == 0 {
            bail!("factory {} has no games on the fork", config.dispute_game_factory_addr);
        }
        let floor = game_count.saturating_sub(config.game_lookback);

        let mut selected = Vec::with_capacity(2);
        for index in (floor..game_count).rev() {
            let game = factory.game_at_index(index).await?;
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
            let Ok(root_count) = u64::try_from(root_count) else {
                continue;
            };
            if root_count == 0 {
                continue;
            }

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
            "need two in-progress, uncountered games of type {} in the newest {} factory \
             indices, found {}; the fork source may be behind or the proposer may be stalled",
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
            Self::tx_manager_config(),
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
    fn release_challenger(
        config: &Config,
        fork_url: &Url,
        signer: &PrivateKeySigner,
    ) -> Result<()> {
        // Sourced after /envmapper/mapping.env, so these override the
        // config-service values for the run.
        let contents = format!(
            "export BASE_CHALLENGER_L1_ETH_RPC={fork_url}\n\
             export BASE_CHALLENGER_PRIVATE_KEY={}\n",
            hex::encode_prefixed(signer.to_bytes())
        );

        let path = &config.challenger_env_file;
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
        info!(window = ?config.quiet_window, "observing the challenger against an unmodified fork");
        tokio::time::sleep(config.quiet_window).await;
        let after = Scrape::fetch(&config.challenger_metrics_url).await?;

        let scanned = after.sum("base_challenger_games_scanned_total")
            - before.sum("base_challenger_games_scanned_total");
        ensure!(
            scanned > 0.0,
            "the challenger scanned no games in {:?}; it is not making progress against the fork",
            config.quiet_window
        );

        for metric in [
            "base_challenger_games_invalid_total",
            "base_challenger_nullify_tx_submitted_total",
            "base_challenger_challenge_tx_submitted_total",
        ] {
            let delta = after.sum(metric) - before.sum(metric);
            ensure!(
                delta == 0.0,
                "{metric} advanced by {delta} while every game on the fork was valid"
            );
        }

        // Validation errors are usually the L2 RPC rather than the challenger,
        // so they are reported rather than fatal. A persistent storm shows up
        // as a dispute timeout below, which repeats this number.
        let errors = after.sum("base_challenger_validation_errors_total")
            - before.sum("base_challenger_validation_errors_total");
        if errors > 0.0 {
            warn!(validation_errors = errors, "the challenger reported validation errors");
        }

        info!(games_scanned = scanned, "the challenger left every valid game alone");
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

        Self::await_dispute(config, verifier, provider, game.address, challenger).await
    }

    /// Path 2 skip: a legitimate ZK challenge of a wrong TEE root must stand.
    ///
    /// Not a failure when Path 1 was a TEE nullify — there is no challenge to
    /// leave alone. Path 2 *dispute* (fraudulent ZK against a correct TEE root)
    /// is not staged: the real prover cannot produce a wrong-root proof the
    /// real verifier accepts.
    async fn run_path2_skip(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        game: Candidate,
        path1: Path1Outcome,
    ) -> Result<()> {
        let Path1Outcome::ZkChallenge = path1 else {
            info!("Path 1 was a TEE nullify; skipping Path 2 skip (not a failure)");
            return Ok(());
        };

        let zk_before = verifier.zk_prover(game.address).await?;
        let countered_before = verifier.countered_index(game.address).await?;
        info!(
            game = %game.address,
            window = ?config.quiet_window,
            zk_prover = %zk_before,
            countered_index = countered_before,
            "observing Path 2 skip on a legitimate ZK challenge"
        );
        tokio::time::sleep(config.quiet_window).await;

        let zk_after = verifier.zk_prover(game.address).await?;
        let countered_after = verifier.countered_index(game.address).await?;
        ensure!(
            zk_after == zk_before,
            "Path 2 skip failed: zkProver changed from {zk_before} to {zk_after} on a \
             legitimate challenge"
        );
        ensure!(
            countered_after == countered_before,
            "Path 2 skip failed: counteredIndex changed from {countered_before} to \
             {countered_after} on a legitimate challenge"
        );

        info!(game = %game.address, "Path 2 skip: legitimate ZK challenge was left standing");
        Ok(())
    }

    /// Path 4 then 3: patch the dual-proof game, wait for TEE nullify, then ZK
    /// nullify. Each step must move B's nonce.
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
        let checkpoint = Checkpoint::patch(&fork_config, verifier)
            .await
            .context("failed to corrupt the dual-proof game on the fork")?;
        info!(
            game = %game.address,
            invalid_index = checkpoint.index,
            start_block = checkpoint.start_block,
            target_block = checkpoint.target_block(),
            "corrupted dual-proof game; waiting for Path 4 then Path 3"
        );

        let mut nonce = provider.get_transaction_count(challenger.address()).await?;

        let tee_cleared = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to TEE-nullify the dual-proof game",
            || async {
                Ok((verifier.tee_prover(game.address).await? == Address::ZERO).then_some(()))
            },
        )
        .await;
        if let Err(error) = tee_cleared {
            return Err(Self::annotate_timeout(error, config).await);
        }
        nonce = Self::assert_challenger_acted(
            provider,
            challenger,
            nonce,
            "TEE-nullified the dual-proof game",
        )
        .await?;
        info!(game = %game.address, "Path 4: TEE proof nullified");

        let zk_cleared = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to ZK-nullify the remaining proof",
            || async {
                Ok((verifier.zk_prover(game.address).await? == Address::ZERO).then_some(()))
            },
        )
        .await;
        if let Err(error) = zk_cleared {
            return Err(Self::annotate_timeout(error, config).await);
        }
        Self::assert_challenger_acted(
            provider,
            challenger,
            nonce,
            "ZK-nullified the remaining proof",
        )
        .await?;
        info!(game = %game.address, "Path 3: ZK proof nullified");
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
    ) -> Result<Path1Outcome> {
        let nonce_before = provider.get_transaction_count(challenger.address()).await?;

        let disputed = Self::poll_until(
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
        .await;
        let outcome = match disputed {
            Ok(outcome) => outcome,
            Err(error) => return Err(Self::annotate_timeout(error, config).await),
        };

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

    fn tx_manager_config() -> TxManagerConfig {
        TxManagerConfig {
            num_confirmations: 1,
            resubmission_timeout: Duration::from_secs(10),
            receipt_query_interval: Duration::from_secs(1),
            tx_send_timeout: Duration::from_secs(180),
            tx_not_in_mempool_timeout: Duration::from_secs(30),
            confirmation_timeout: Duration::from_secs(120),
            ..Default::default()
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
                match last_error {
                    Some(error) => Err(error.wrap_err(msg)),
                    None => bail!("{msg}"),
                }
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
