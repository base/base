//! Drives a real challenger binary against a throwaway fork of the target L1.

use std::{path::Path, time::Duration};

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, U256, hex};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
    GameStatus,
};
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

/// Behavioural end-to-end test of the challenger.
///
/// See the crate README for the full argument; the short version is that the
/// fork is built from a real chain and the games under test were created and
/// verified on that chain, so nothing the challenger sees is stubbed.
#[derive(Debug)]
pub struct ChallengerE2e;

impl ChallengerE2e {
    /// Runs the test to completion. An `Ok` return means the challenger passed.
    pub async fn run() -> Result<()> {
        let config = Config::parse();

        // Generated per run and never leaves the pod. The dispute paths add a
        // second account that signs their setup; this one only ever disputes.
        let challenger = PrivateKeySigner::random();

        // Held until the end of run(); the fork dies with this binding.
        let anvil = Self::spawn_fork(&config)?;
        let fork_url = anvil.endpoint_url();
        let provider: RootProvider = RootProvider::new_http(fork_url.clone());
        Self::fund(&provider, &[challenger.address()]).await?;

        let factory = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            provider.clone(),
        );
        let verifier = AggregateVerifierContractClient::new(provider.clone());
        let anchor_registry = AnchorStateRegistryContractClient::new(
            config.anchor_state_registry_addr,
            provider.clone(),
        );

        // Chosen before the challenger boots, so the quiet window below is
        // measured against a fork that already contains the target games.
        let (game_a, game_b) =
            Self::select_games(&config, &factory, &verifier, &anchor_registry).await?;

        Self::release_challenger(&fork_url, &challenger)?;
        Self::await_first_scan(&config).await?;

        Self::assert_quiet_on_valid_games(&config).await?;

        info!(
            game_a = %game_a,
            game_b = %game_b,
            "the fork holds two disputable games and the challenger left both alone"
        );
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
    /// Selecting them here rather than later is what makes the quiet window a
    /// real assertion: the games the challenger must ignore are the same ones
    /// it will later be asked to dispute. Scanning newest-first also keeps the
    /// range recent, which matters because the L2 RPC is a live node and may
    /// have pruned the state behind an older game.
    async fn select_games(
        config: &Config,
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
        anchor_registry: &AnchorStateRegistryContractClient,
    ) -> Result<(Address, Address)> {
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
            // Corrupting a game means rewriting one of its intermediate roots,
            // so a game without any is not disputable by this test.
            let root_count = verifier.intermediate_output_roots(game.proxy).await?.len();
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
            selected.push(game.proxy);
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
    /// Nothing on the fork has been corrupted yet, so every game the
    /// challenger can see is one it must leave alone.
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
