use std::{env, time::Duration};

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use alloy_provider::{fillers::TxFiller, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use clap::Parser;
use op_alloy_network::EthereumWallet;
use tokio::time;

use fault_proof::{
    config::ChallengerConfig,
    contract::{
        DisputeGameFactory::{self, DisputeGameFactoryInstance},
        OPSuccinctFaultDisputeGame,
    },
    prometheus::ChallengerGauge,
    utils::setup_logging,
    Action, FactoryTrait, L1ProviderWithWallet, L2Provider, Mode, NUM_CONFIRMATIONS,
    TIMEOUT_SECONDS,
};
use op_succinct_host_utils::metrics::{init_metrics, MetricsGauge};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.challenger")]
    env_file: String,
}

struct OPSuccinctChallenger<F, P>
where
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum> + Clone,
{
    config: ChallengerConfig,
    challenger_address: Address,
    l2_provider: L2Provider,
    l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
    factory: DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>,
    challenger_bond: U256,
}

impl<F, P> OPSuccinctChallenger<F, P>
where
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum> + Clone,
{
    /// Creates a new challenger instance with the provided L1 provider with wallet and factory contract instance.
    pub async fn new(
        challenger_address: Address,
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        factory: DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>,
    ) -> Result<Self> {
        let config = ChallengerConfig::from_env()?;

        Ok(Self {
            config: config.clone(),
            challenger_address,
            l2_provider: ProviderBuilder::default().on_http(config.l2_rpc.clone()),
            l1_provider_with_wallet: l1_provider_with_wallet.clone(),
            factory: factory.clone(),
            challenger_bond: factory.fetch_challenger_bond(config.game_type).await?,
        })
    }

    /// Challenges a specific game at the given address.
    async fn challenge_game(&self, game_address: Address) -> Result<()> {
        let game =
            OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider_with_wallet.clone());

        let receipt = game
            .challenge()
            .value(self.challenger_bond)
            .send()
            .await
            .context("Failed to send challenge transaction")?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await
            .context("Failed to get transaction receipt for challenge")?;

        tracing::info!(
            "Successfully challenged game {:?} with tx {:?}",
            game_address,
            receipt.transaction_hash
        );

        Ok(())
    }

    /// Handles challenging of invalid games by scanning recent games for potential challenges.
    async fn handle_game_challenging(&self) -> Result<Action> {
        let _span = tracing::info_span!("[[Challenging]]").entered();

        if let Some(game_address) = self
            .factory
            .get_oldest_challengable_game_address(
                self.config.max_games_to_check_for_challenge,
                self.l2_provider.clone(),
            )
            .await?
        {
            tracing::info!("Attempting to challenge game {:?}", game_address);
            self.challenge_game(game_address).await?;
            Ok(Action::Performed)
        } else {
            Ok(Action::Skipped)
        }
    }

    /// Handles resolution of challenged games that are ready to be resolved.
    async fn handle_game_resolution(&self) -> Result<()> {
        let _span = tracing::info_span!("[[Resolving]]").entered();

        self.factory
            .resolve_games(
                Mode::Challenger,
                self.config.max_games_to_check_for_resolution,
                self.l1_provider_with_wallet.clone(),
                self.l2_provider.clone(),
            )
            .await
    }

    /// Handles claiming bonds from resolved games.
    pub async fn handle_bond_claiming(&self) -> Result<Action> {
        let _span = tracing::info_span!("[[Claiming Bonds]]").entered();

        if let Some(game_address) = self
            .factory
            .get_oldest_claimable_bond_game_address(
                self.config.game_type,
                self.config.max_games_to_check_for_bond_claiming,
                self.challenger_address,
            )
            .await?
        {
            tracing::info!("Attempting to claim bond from game {:?}", game_address);

            // Create a contract instance for the game
            let game =
                OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider_with_wallet.clone());

            // Create a transaction to claim credit
            let tx = game.claimCredit(self.challenger_address);

            // Send the transaction
            match tx.send().await {
                Ok(pending_tx) => {
                    let receipt = pending_tx
                        .with_required_confirmations(NUM_CONFIRMATIONS)
                        .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
                        .get_receipt()
                        .await?;

                    tracing::info!(
                        "\x1b[1mSuccessfully claimed bond from game {:?} with tx {:?}\x1b[0m",
                        game_address,
                        receipt.transaction_hash
                    );

                    Ok(Action::Performed)
                }
                Err(e) => Err(anyhow::anyhow!(
                    "Failed to claim bond from game {:?}: {:?}",
                    game_address,
                    e
                )),
            }
        } else {
            tracing::info!("No new games to claim bonds from");

            Ok(Action::Skipped)
        }
    }

    /// Runs the challenger in an infinite loop, periodically checking for games to challenge and resolve.
    async fn run(&mut self) -> Result<()> {
        tracing::info!("OP Succinct Challenger running...");
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        // Each loop, check the oldest challengeable game and challenge it if it exists.
        // Eventually, all games will be challenged (as long as the rate at which games are being created is slower than the fetch interval).
        loop {
            interval.tick().await;

            match self.handle_game_challenging().await {
                Ok(Action::Performed) => {
                    ChallengerGauge::GamesChallenged.increment(1.0);
                }
                Ok(Action::Skipped) => {}
                Err(e) => {
                    tracing::warn!("Failed to handle game challenging: {:?}", e);
                    ChallengerGauge::Errors.increment(1.0);
                }
            }

            match self.handle_game_resolution().await {
                Ok(_) => {
                    ChallengerGauge::GamesResolved.increment(1.0);
                }
                Err(e) => {
                    tracing::warn!("Failed to handle game resolution: {:?}", e);
                    ChallengerGauge::Errors.increment(1.0);
                }
            }

            match self.handle_bond_claiming().await {
                Ok(Action::Performed) => {
                    ChallengerGauge::GamesBondsClaimed.increment(1.0);
                }
                Ok(Action::Skipped) => {}
                Err(e) => {
                    tracing::warn!("Failed to handle bond claiming: {:?}", e);
                    ChallengerGauge::Errors.increment(1.0);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let wallet = EthereumWallet::from(
        env::var("PRIVATE_KEY")
            .expect("PRIVATE_KEY must be set")
            .parse::<PrivateKeySigner>()
            .unwrap(),
    );

    let l1_provider_with_wallet = ProviderBuilder::new()
        .wallet(wallet.clone())
        .on_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider_with_wallet.clone(),
    );

    let mut challenger = OPSuccinctChallenger::new(
        wallet.default_signer().address(),
        l1_provider_with_wallet,
        factory,
    )
    .await
    .unwrap();

    // Initialize challenger gauges.
    ChallengerGauge::register_all();

    // Initialize metrics exporter.
    init_metrics(&challenger.config.metrics_port);

    // Initialize the metrics gauges.
    ChallengerGauge::init_all();

    challenger.run().await.expect("Runs in an infinite loop");

    Ok(())
}
