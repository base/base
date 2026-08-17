//! Drives a real challenger binary against a throwaway fork of the target L1.

use std::time::Duration;

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, U256, hex};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameStatus,
};
use base_proof_rpc::L2HttpProvider;
use base_prover_service_protocol::ZkBackend;
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

/// Behavioural end-to-end test of the challenger.
///
/// See the crate README for the full argument; the short version is that the
/// fork is built from a real chain and the game under test was created and
/// verified on that chain, so nothing about the dispute is stubbed.
#[derive(Debug)]
pub struct ChallengerE2e;

impl ChallengerE2e {
    /// Runs the test to completion. An `Ok` return means the challenger passed.
    pub async fn run() -> Result<()> {
        let config = Config::parse();

        // Two distinct accounts: the driver never signs anything, so "the
        // challenger disputed the game" stays distinguishable from "the driver
        // did". Both are generated per run and never leave the pod.
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
        // measured against a fork that already contains the target game.
        let candidate = Self::select_game(&config, &factory, &verifier).await?;

        Self::release_challenger(&config, &fork_url, &challenger)?;
        Self::await_first_scan(&config).await?;

        Self::assert_quiet_on_valid_games(&config).await?;

        let fork_config = Self::fork_config(&config, &fork_url, &driver, candidate);
        let checkpoint = Checkpoint::patch(&fork_config, &verifier)
            .await
            .context("failed to corrupt an intermediate output root on the fork")?;
        info!(
            game = %candidate.address,
            invalid_index = checkpoint.index,
            start_block = checkpoint.start_block,
            target_block = checkpoint.target_block(),
            "corrupted intermediate output root; waiting for the challenger to dispute"
        );

        Self::await_dispute(&config, &verifier, &provider, candidate.address, &challenger).await?;

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

    /// Picks the newest in-progress TEE-only game the challenger will classify
    /// as disputable once its roots stop matching L2.
    ///
    /// Scanning newest-first also keeps the corrupted range recent, which
    /// matters because the L2 RPC is a live node and may have pruned the state
    /// behind an older game.
    async fn select_game(
        config: &Config,
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
    ) -> Result<Candidate> {
        let game_count = factory.game_count().await?;
        if game_count == 0 {
            bail!("factory {} has no games on the fork", config.dispute_game_factory_addr);
        }
        let floor = game_count.saturating_sub(config.game_lookback);

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

            info!(game = %game.proxy, factory_index = index, root_count, "selected game to corrupt");
            return Ok(Candidate { address: game.proxy, root_count });
        }

        bail!(
            "no in-progress, uncountered game of type {} in the newest {} factory indices; \
             the fork source may be behind or the proposer may be stalled",
            config.game_type,
            game_count - floor
        )
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
    ) -> Result<()> {
        let nonce_before = provider.get_transaction_count(challenger.address()).await?;

        let disputed = Self::poll_until(
            config,
            config.dispute_timeout,
            "the challenger to dispute the corrupted game",
            || async {
                if verifier.tee_prover(game).await? == Address::ZERO {
                    return Ok(Some("nullified via TEE proof"));
                }
                let countered = verifier.countered_index(game).await? != 0;
                if countered && verifier.zk_prover(game).await? != Address::ZERO {
                    return Ok(Some("challenged via ZK proof"));
                }
                Ok(None)
            },
        )
        .await;
        let outcome = match disputed {
            Ok(outcome) => outcome,
            Err(error) => return Err(Self::annotate_timeout(error, config).await),
        };

        // ponytail: a nonce bump plus the state change is enough to attribute
        // the dispute — the driver signs nothing, so nothing else on the fork
        // could have moved it. Walk the mined blocks for the calling address if
        // this ever needs to name the exact transaction.
        let nonce_after = provider.get_transaction_count(challenger.address()).await?;
        ensure!(
            nonce_after > nonce_before,
            "the game was {outcome} but the challenger's nonce is unchanged at {nonce_before}; \
             something other than the challenger disputed it"
        );

        info!(
            game = %game,
            outcome,
            transactions = nonce_after - nonce_before,
            "the challenger disputed the corrupted game"
        );
        Ok(())
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
            // Unused: only `Checkpoint::request_proof` talks to the prover
            // service, and the challenger is the one requesting proofs here.
            prover_service_url: fork_url.clone(),
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
