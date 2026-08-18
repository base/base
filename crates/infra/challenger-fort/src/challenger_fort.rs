//! Observes the live Challenger against games that already exist on live L1.

use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::RootProvider;
use base_challenger::{IntermediateValidationParams, OutputValidator, ValidatorError};
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameStatus,
};
use base_proof_rpc::{L2Client, L2ClientConfig};
use clap::Parser;
use eyre::{Context, Result, bail, ensure};
use tracing::{info, warn};

use crate::{config::Config, metrics::Scrape};

/// Action counters that must stay flat when every observed game is good.
const ACTION_METRICS: [&str; 3] = [
    "base_challenger_games_invalid_total",
    "base_challenger_nullify_tx_submitted_total",
    "base_challenger_challenge_tx_submitted_total",
];

/// On-chain prover / countered state used to classify a game and to judge
/// whether it was later disputed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProverState {
    tee: Address,
    zk: Address,
    countered: u64,
}

/// Classifier path that decides what on-chain change counts as a dispute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// TEE-only, unchallenged (`tee != 0`, `zk == 0`, `countered == 0`).
    TeeOnly,
    /// TEE + ZK, already challenged (`countered > 0`).
    Challenged,
    /// ZK-only, unchallenged (`tee == 0`, `zk != 0`, `countered == 0`).
    ZkOnly,
    /// TEE + ZK, no challenge (`countered == 0`).
    Dual,
}

/// A live in-progress game FORT will assert on.
#[derive(Debug, Clone, Copy)]
struct ObservedGame {
    index: u64,
    address: Address,
    path: Path,
    kind: Kind,
}

/// Whether the game's claimed roots match L2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Intermediate roots match L2. The Challenger must leave it alone.
    Good,
    /// Intermediate roots do not match L2. The Challenger must dispute it.
    Bad,
    /// Path 2: original root at the challenged index is correct. The Challenger
    /// must nullify the fraudulent ZK challenge.
    FraudulentChallenge,
}

/// Live-chain observer for the challenger after deploy.
///
/// See the crate README for the full argument. FORT never plants a game and
/// never forks L1; it only reads the factory and the live Challenger's metrics.
#[derive(Debug)]
pub struct ChallengerFort;

impl ChallengerFort {
    /// Runs the observer to completion. An `Ok` return means pass or skip.
    pub async fn run() -> Result<()> {
        let config = Config::parse();

        let l1 = RootProvider::new_http(config.l1_eth_rpc.clone());
        let factory =
            DisputeGameFactoryContractClient::new(config.dispute_game_factory_addr, l1.clone());
        let verifier = AggregateVerifierContractClient::new(l1);
        let l2 = Arc::new(
            L2Client::new(L2ClientConfig::new(config.l2_eth_rpc.clone()))
                .context("failed to build an L2 client")?,
        );
        let validator = OutputValidator::new(l2);

        let games = Self::collect_games(&config, &factory, &verifier, &validator).await?;
        if games.is_empty() {
            return Ok(());
        }

        let all_good = games.iter().all(|game| game.kind == Kind::Good);
        let baseline = Scrape::fetch(&config.challenger_metrics_url).await.ok();

        info!(
            good = games.iter().filter(|game| game.kind == Kind::Good).count(),
            bad = games.iter().filter(|game| game.kind == Kind::Bad).count(),
            fraudulent_challenge =
                games.iter().filter(|game| game.kind == Kind::FraudulentChallenge).count(),
            window = ?config.window,
            "observing the live challenger"
        );

        Self::await_pass(&config, &verifier, &games, baseline.as_ref(), all_good).await?;

        let addresses: Vec<_> = games.iter().map(|game| game.address).collect();
        info!(
            games = games.len(),
            good = games.iter().filter(|game| game.kind == Kind::Good).count(),
            bad = games.iter().filter(|game| game.kind == Kind::Bad).count(),
            ?addresses,
            "challenger FORT passed"
        );
        Ok(())
    }

    /// Newest in-progress games of the configured type, classified against L2.
    ///
    /// Returns an empty vec (and logs skip) when nothing is actionable: no
    /// matching games, or every candidate was pruned / unvalidatable.
    async fn collect_games(
        config: &Config,
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
        validator: &OutputValidator<L2Client>,
    ) -> Result<Vec<ObservedGame>> {
        let game_count = factory.game_count().await?;
        if game_count == 0 {
            info!(
                factory = %config.dispute_game_factory_addr,
                "factory has no games; skipping (proposer stall is not a challenger fail)"
            );
            return Ok(Vec::new());
        }

        let floor = game_count.saturating_sub(config.game_lookback);
        let mut games = Vec::new();
        let mut skipped = 0u64;
        let mut interval = None;

        for index in (floor..game_count).rev() {
            let factory_game = factory.game_at_index(index).await?;
            if factory_game.game_type != config.game_type {
                continue;
            }
            if verifier.status(factory_game.proxy).await? != GameStatus::InProgress {
                continue;
            }

            let state = Self::prover_state(verifier, factory_game.proxy).await?;
            let Some(path) = Path::classify(state) else {
                skipped += 1;
                continue;
            };

            let interval = match interval {
                Some(value) => value,
                None => {
                    let resolved =
                        Self::intermediate_block_interval(factory, verifier, config.game_type)
                            .await?;
                    interval = Some(resolved);
                    resolved
                }
            };

            match Self::observe(validator, verifier, factory_game.proxy, index, path, interval)
                .await?
            {
                Some(game) => {
                    info!(
                        game = %game.address,
                        factory_index = game.index,
                        path = ?game.path,
                        kind = ?game.kind,
                        "observing in-progress game"
                    );
                    games.push(game);
                }
                None => skipped += 1,
            }
        }

        if games.is_empty() {
            info!(
                lookback = config.game_lookback,
                game_type = config.game_type,
                factory = %config.dispute_game_factory_addr,
                skipped,
                "no actionable in-progress games; skipping (proposer stall or L2 prune is not a challenger fail)"
            );
        }

        Ok(games)
    }

    async fn intermediate_block_interval(
        factory: &DisputeGameFactoryContractClient,
        verifier: &AggregateVerifierContractClient,
        game_type: u32,
    ) -> Result<u64> {
        let impl_address = factory.game_impls(game_type).await?;
        ensure!(
            impl_address != Address::ZERO,
            "no game implementation registered in DisputeGameFactory for game type {game_type}"
        );
        let interval = verifier.read_intermediate_block_interval(impl_address).await?;
        ensure!(interval != 0, "INTERMEDIATE_BLOCK_INTERVAL must be non-zero");
        Ok(interval)
    }

    async fn prover_state(
        verifier: &AggregateVerifierContractClient,
        game: Address,
    ) -> Result<ProverState> {
        let (tee, zk, countered) = tokio::try_join!(
            verifier.tee_prover(game),
            verifier.zk_prover(game),
            verifier.countered_index(game),
        )?;
        Ok(ProverState { tee, zk, countered })
    }

    async fn observe(
        validator: &OutputValidator<L2Client>,
        verifier: &AggregateVerifierContractClient,
        address: Address,
        index: u64,
        path: Path,
        interval: u64,
    ) -> Result<Option<ObservedGame>> {
        if path == Path::Challenged {
            return Self::observe_path2(validator, verifier, address, index, interval).await;
        }

        let (info, starting_block, intermediate_roots) = tokio::try_join!(
            verifier.game_info(address),
            verifier.starting_block_number(address),
            verifier.intermediate_output_roots(address),
        )?;

        let params = IntermediateValidationParams {
            game_address: address,
            starting_block_number: starting_block,
            l2_block_number: info.l2_block_number,
            intermediate_block_interval: interval,
            claimed_root: info.root_claim,
            intermediate_roots: &intermediate_roots,
        };

        match validator.validate_intermediate_roots(params).await {
            Ok(result) => {
                let kind = if result.is_valid { Kind::Good } else { Kind::Bad };
                Ok(Some(ObservedGame { index, address, path, kind }))
            }
            Err(error) => {
                skip_or_fail(address, index, error)?;
                Ok(None)
            }
        }
    }

    async fn observe_path2(
        validator: &OutputValidator<L2Client>,
        verifier: &AggregateVerifierContractClient,
        address: Address,
        index: u64,
        interval: u64,
    ) -> Result<Option<ObservedGame>> {
        let state = Self::prover_state(verifier, address).await?;
        let challenged_index = state.countered.saturating_sub(1);
        let (on_chain_root, starting_block) = tokio::try_join!(
            verifier.intermediate_output_root(address, challenged_index),
            verifier.starting_block_number(address),
        )?;
        let Some(checkpoint_block) = checkpoint_block(starting_block, interval, challenged_index)
        else {
            warn!(game = %address, challenged_index, "checkpoint arithmetic overflow; skipping");
            return Ok(None);
        };

        match validator
            .validate_claimed_root_at_block(address, checkpoint_block, on_chain_root)
            .await
        {
            Ok(result) if result.is_valid => Ok(Some(ObservedGame {
                index,
                address,
                path: Path::Challenged,
                kind: Kind::FraudulentChallenge,
            })),
            Ok(_) => {
                info!(
                    game = %address,
                    factory_index = index,
                    challenged_index,
                    "skipping Path 2 game; original root is wrong (legitimate challenge)"
                );
                Ok(None)
            }
            Err(error) => {
                skip_or_fail(address, index, error)?;
                Ok(None)
            }
        }
    }

    fn evaluate(
        games: &[ObservedGame],
        states: &[ProverState],
        scrape: &Scrape,
        baseline: Option<&Scrape>,
        all_good: bool,
    ) -> Eval {
        debug_assert_eq!(games.len(), states.len());

        let mut pending = Vec::new();
        for (game, state) in games.iter().zip(states) {
            if !game_was_scanned(scrape, game.index) {
                pending.push(format!("{} not scanned", game.address));
                continue;
            }
            match game.kind {
                Kind::Good => {
                    if !state.is_undisputed(game.path) {
                        return Eval::Fail(eyre::eyre!(
                            "good game {} was disputed (tee={} zk={} countered={})",
                            game.address,
                            state.tee,
                            state.zk,
                            state.countered
                        ));
                    }
                }
                Kind::Bad => {
                    if !state.is_disputed(game.path) {
                        pending.push(format!("{} not disputed", game.address));
                    }
                }
                Kind::FraudulentChallenge => {
                    if state.zk != Address::ZERO {
                        pending.push(format!(
                            "{} fraudulent ZK challenge not nullified",
                            game.address
                        ));
                    }
                }
            }
        }

        if !pending.is_empty() {
            return Eval::Pending(pending);
        }

        if all_good && let Some(before) = baseline {
            for metric in ACTION_METRICS {
                let delta = scrape.sum(metric) - before.sum(metric);
                if delta.round() as i64 != 0 {
                    return Eval::Fail(eyre::eyre!(
                        "{metric} advanced by {delta} while every observed game was good"
                    ));
                }
            }
        }

        Eval::Pass
    }

    /// Polls L1 and metrics until pass, a terminal fail, or the window elapses.
    ///
    /// A missing scan is pending, not a fail. Transient RPC/metrics errors are
    /// retried. A good game that was disputed, or action counters moving on an
    /// all-good window, fail immediately.
    async fn await_pass(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        games: &[ObservedGame],
        baseline: Option<&Scrape>,
        all_good: bool,
    ) -> Result<()> {
        let mut last_error = None;
        let mut last_pending = Vec::new();
        match tokio::time::timeout(config.window, async {
            loop {
                match Self::tick(config, verifier, games, baseline, all_good).await {
                    Ok(Eval::Pass) => return Ok(()),
                    Ok(Eval::Pending(pending)) => last_pending = pending,
                    Ok(Eval::Fail(error)) => return Err(error),
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
                let msg = format!(
                    "timed out after {:?} waiting for the live challenger to satisfy FORT",
                    config.window
                );
                if !last_pending.is_empty() {
                    bail!("{msg}: {}", last_pending.join("; "));
                }
                match last_error {
                    Some(error) => Err(error.wrap_err(msg)),
                    None => bail!("{msg}"),
                }
            }
        }
    }

    async fn tick(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        games: &[ObservedGame],
        baseline: Option<&Scrape>,
        all_good: bool,
    ) -> Result<Eval> {
        let scrape = Scrape::fetch(&config.challenger_metrics_url).await?;
        let mut states = Vec::with_capacity(games.len());
        for game in games {
            states.push(Self::prover_state(verifier, game.address).await?);
        }
        Ok(Self::evaluate(games, &states, &scrape, baseline, all_good))
    }
}

/// Outcome of one evaluation tick.
#[derive(Debug)]
enum Eval {
    Pass,
    Pending(Vec<String>),
    Fail(eyre::Report),
}

impl Path {
    fn classify(state: ProverState) -> Option<Self> {
        let has_tee = state.tee != Address::ZERO;
        let has_zk = state.zk != Address::ZERO;
        match (has_tee, has_zk, state.countered) {
            (true, false, 0) => Some(Self::TeeOnly),
            (true, true, 0) => Some(Self::Dual),
            (true, true, ci) if ci > 0 => Some(Self::Challenged),
            (false, true, 0) => Some(Self::ZkOnly),
            _ => None,
        }
    }
}

impl ProverState {
    /// Still the undisputed shape for this path.
    fn is_undisputed(self, path: Path) -> bool {
        match path {
            Path::TeeOnly => {
                self.tee != Address::ZERO && self.zk == Address::ZERO && self.countered == 0
            }
            Path::ZkOnly => {
                self.tee == Address::ZERO && self.zk != Address::ZERO && self.countered == 0
            }
            Path::Dual => {
                self.tee != Address::ZERO && self.zk != Address::ZERO && self.countered == 0
            }
            Path::Challenged => false,
        }
    }

    /// On-chain dispute matching this path's expected shape.
    fn is_disputed(self, path: Path) -> bool {
        match path {
            Path::TeeOnly => path1_disputed(self.tee, self.zk, self.countered),
            Path::ZkOnly | Path::Challenged => self.zk == Address::ZERO,
            Path::Dual => self.tee == Address::ZERO,
        }
    }
}

fn path1_disputed(tee: Address, zk: Address, countered: u64) -> bool {
    tee == Address::ZERO || (zk != Address::ZERO && countered != 0)
}

fn game_was_scanned(scrape: &Scrape, index: u64) -> bool {
    let scanned = scrape.sum("base_challenger_games_scanned_total");
    if scanned <= 0.0 {
        return false;
    }
    let head = scrape.sum("base_challenger_scan_head");
    // A finished scan sets `scan_head` to the last factory index. Absent
    // `scan_head` (sum 0) still counts as coverage when the counter moved:
    // one scan evaluates the whole post-anchor range, which includes the
    // lookback tail FORT inspects.
    head == 0.0 || head as u64 >= index
}

/// Skips only L2 prune / transient RPC. Other validator errors are real
/// divergence and fail the run.
fn skip_or_fail(address: Address, index: u64, error: ValidatorError) -> Result<()> {
    match error {
        ValidatorError::BlockNotAvailable { .. } | ValidatorError::Rpc(_) => {
            info!(
                game = %address,
                factory_index = index,
                error = %error,
                "L2 prune or block not available; skipping game"
            );
            Ok(())
        }
        other => Err(other).wrap_err(format!(
            "validation failed for game {address} at factory index {index}"
        )),
    }
}

fn checkpoint_block(starting_block: u64, interval: u64, challenged_index: u64) -> Option<u64> {
    let offset = interval.checked_mul(challenged_index.checked_add(1)?)?;
    starting_block.checked_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    #[test]
    fn path1_disputed_tee_cleared_or_zk_challenge() {
        assert!(path1_disputed(Address::ZERO, Address::ZERO, 0));
        assert!(path1_disputed(Address::ZERO, addr(0xCC), 0));
        assert!(path1_disputed(addr(0xEE), addr(0xCC), 3));
        assert!(!path1_disputed(addr(0xEE), Address::ZERO, 0));
    }

    #[test]
    fn good_path1_stays_undisputed() {
        let state = ProverState { tee: addr(0xEE), zk: Address::ZERO, countered: 0 };
        assert!(state.is_undisputed(Path::TeeOnly));
        assert!(!state.is_disputed(Path::TeeOnly));
    }

    #[test]
    fn path3_and_path4_dispute_shapes() {
        let path3 = ProverState { tee: Address::ZERO, zk: addr(0xCC), countered: 0 };
        assert!(path3.is_undisputed(Path::ZkOnly));
        assert!(
            ProverState { tee: Address::ZERO, zk: Address::ZERO, countered: 0 }
                .is_disputed(Path::ZkOnly)
        );

        let path4 = ProverState { tee: addr(0xEE), zk: addr(0xCC), countered: 0 };
        assert!(path4.is_undisputed(Path::Dual));
        assert!(
            ProverState { tee: Address::ZERO, zk: addr(0xCC), countered: 0 }
                .is_disputed(Path::Dual)
        );
    }

    #[test]
    fn scan_coverage_requires_head_or_counter() {
        let scrape =
            Scrape::parse("base_challenger_scan_head 40\nbase_challenger_games_scanned_total 10\n");
        assert!(game_was_scanned(&scrape, 40));
        assert!(game_was_scanned(&scrape, 10));
        assert!(!game_was_scanned(&scrape, 41));

        let counter_only = Scrape::parse("base_challenger_games_scanned_total 3\n");
        assert!(game_was_scanned(&counter_only, 5));
    }

    #[test]
    fn evaluate_pending_until_scanned() {
        let game =
            ObservedGame { index: 7, address: addr(0x11), path: Path::TeeOnly, kind: Kind::Good };
        let state = ProverState { tee: addr(0xEE), zk: Address::ZERO, countered: 0 };
        let scrape = Scrape::parse("base_challenger_games_scanned_total 0\n");
        assert!(matches!(
            ChallengerFort::evaluate(&[game], &[state], &scrape, None, true),
            Eval::Pending(_)
        ));
    }

    #[test]
    fn evaluate_fails_when_good_game_is_disputed() {
        let game =
            ObservedGame { index: 7, address: addr(0x11), path: Path::TeeOnly, kind: Kind::Good };
        let state = ProverState { tee: Address::ZERO, zk: Address::ZERO, countered: 0 };
        let scrape =
            Scrape::parse("base_challenger_scan_head 7\nbase_challenger_games_scanned_total 1\n");
        assert!(matches!(
            ChallengerFort::evaluate(&[game], &[state], &scrape, None, true),
            Eval::Fail(_)
        ));
    }
}
