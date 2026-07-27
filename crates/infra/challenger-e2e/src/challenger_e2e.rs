//! Challenger E2E observation implementation.

use std::time::{Duration, Instant};

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use base_proof_contracts::{DisputeGameFactoryClient, DisputeGameFactoryContractClient, GameAtIndex};
use clap::Parser;
use eyre::{Context, Result, bail};
use url::Url;

/// Challenger E2E observation config.
#[derive(Debug, Parser)]
pub struct ChallengerE2eConfig {
    /// L1 RPC URL used by Challenger.
    #[arg(long, env = "CHALLENGER_L1_RPC_URL")]
    pub l1_rpc_url: Url,

    /// DisputeGameFactory address watched by Challenger.
    #[arg(long, env = "CHALLENGER_DISPUTE_GAME_FACTORY_ADDR")]
    pub dispute_game_factory_addr: Address,

    /// Optional game type filter.
    #[arg(long, env = "CHALLENGER_GAME_TYPE")]
    pub game_type: Option<u32>,

    /// Maximum time to wait for a new dispute game after the gate starts.
    #[arg(long, env = "CHALLENGER_NEW_GAME_TIMEOUT_SECS", default_value_t = 30 * 60)]
    pub new_game_timeout_secs: u64,

    /// Poll interval while waiting for a new dispute game.
    #[arg(long, env = "CHALLENGER_POLL_INTERVAL_SECS", default_value_t = 30)]
    pub poll_interval_secs: u64,

    /// Maximum number of recent games to scan when looking for a matching game type.
    #[arg(long, env = "CHALLENGER_LATEST_GAME_SCAN_LIMIT", default_value_t = 100)]
    pub latest_game_scan_limit: u64,
}

/// Challenger E2E observation runner.
#[derive(Debug)]
pub struct ChallengerE2e;

impl ChallengerE2e {
    /// Runs the Challenger E2E observation.
    pub async fn run(config: ChallengerE2eConfig) -> Result<()> {
        let provider = RootProvider::new_http(config.l1_rpc_url.clone());
        Self::assert_contract(&provider, config.dispute_game_factory_addr, "DisputeGameFactory")
            .await?;

        let factory = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            config.l1_rpc_url.clone(),
        )?;

        let initial_count = factory.game_count().await.context("gameCount before wait failed")?;
        if initial_count == 0 {
            bail!("DisputeGameFactory has no games");
        }

        let latest = Self::latest_game(&factory, initial_count, config.game_type, config.latest_game_scan_limit)
            .await?
            .ok_or_else(|| eyre::eyre!("no dispute game found for game type {:?} in last {} games", config.game_type, config.latest_game_scan_limit))?;
        Self::assert_contract(&provider, latest.proxy, "latest dispute game").await?;
        tracing::info!(
            game_count = initial_count,
            game_type = latest.game_type,
            timestamp = latest.timestamp,
            proxy = %latest.proxy,
            "observed latest dispute game before wait",
        );

        let deadline = Instant::now() + Duration::from_secs(config.new_game_timeout_secs);
        let poll_interval = Duration::from_secs(config.poll_interval_secs);
        while Instant::now() < deadline {
            let count = factory.game_count().await.context("gameCount while waiting failed")?;
            if count > initial_count {
                if let Some(game) = Self::latest_game_in_range(
                    &factory,
                    initial_count,
                    count,
                    config.game_type,
                    config.latest_game_scan_limit,
                )
                .await?
                {
                    Self::assert_contract(&provider, game.proxy, "new dispute game").await?;
                    tracing::info!(
                        previous_game_count = initial_count,
                        new_game_count = count,
                        game_type = game.game_type,
                        timestamp = game.timestamp,
                        proxy = %game.proxy,
                        "observed new dispute game after gate start",
                    );
                    return Ok(());
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        bail!("no new dispute game observed within {}s", config.new_game_timeout_secs)
    }

    async fn latest_game(
        factory: &DisputeGameFactoryContractClient,
        game_count: u64,
        game_type: Option<u32>,
        scan_limit: u64,
    ) -> Result<Option<GameAtIndex>> {
        Self::latest_game_in_range(
            factory,
            game_count.saturating_sub(scan_limit),
            game_count,
            game_type,
            scan_limit,
        )
        .await
    }

    async fn latest_game_in_range(
        factory: &DisputeGameFactoryContractClient,
        from: u64,
        to: u64,
        game_type: Option<u32>,
        scan_limit: u64,
    ) -> Result<Option<GameAtIndex>> {
        let lower = from.max(to.saturating_sub(scan_limit));
        for index in (lower..to).rev() {
            let game = factory.game_at_index(index).await?;
            if game_type.is_none_or(|expected| game.game_type == expected) {
                return Ok(Some(game));
            }
        }
        Ok(None)
    }

    async fn assert_contract(provider: &RootProvider, address: Address, name: &str) -> Result<()> {
        if provider.get_code_at(address).await?.is_empty() {
            bail!("{name} {address} has no code");
        }
        Ok(())
    }
}
