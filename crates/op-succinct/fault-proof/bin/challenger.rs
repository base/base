use std::{env, time::Duration};

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_transport_http::reqwest::Url;
use anyhow::Result;
use clap::Parser;
use fault_proof::{
    config::ChallengerConfig,
    contract::{
        DisputeGameFactory::{self, DisputeGameFactoryInstance},
        OPSuccinctFaultDisputeGame,
    },
    prometheus::ChallengerGauge,
    Action, FactoryTrait, L1Provider, L2Provider, Mode,
};
use op_succinct_host_utils::{
    metrics::{init_metrics, MetricsGauge},
    setup_logger,
};
use op_succinct_signer_utils::Signer;
use rand::Rng;
use tokio::time;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = ".env.challenger")]
    env_file: String,
}

struct OPSuccinctChallenger<P>
where
    P: Provider + Clone,
{
    config: ChallengerConfig,
    challenger_address: Address,
    signer: Signer,
    l1_provider: L1Provider,
    l2_provider: L2Provider,
    factory: DisputeGameFactoryInstance<P>,
    challenger_bond: U256,
}

impl<P> OPSuccinctChallenger<P>
where
    P: Provider + Clone,
{
    /// Creates a new challenger instance with the provided L1 provider with wallet and factory
    /// contract instance.
    pub async fn new(
        challenger_address: Address,
        signer: Signer,
        l1_provider: L1Provider,
        factory: DisputeGameFactoryInstance<P>,
    ) -> Result<Self> {
        let config = ChallengerConfig::from_env()?;

        Ok(Self {
            config: config.clone(),
            challenger_address,
            signer,
            l1_provider: l1_provider.clone(),
            l2_provider: ProviderBuilder::default().connect_http(config.l2_rpc.clone()),
            factory: factory.clone(),
            challenger_bond: factory.fetch_challenger_bond(config.game_type).await?,
        })
    }

    /// Challenges a specific game at the given address.
    async fn challenge_game(&self, game_address: Address) -> Result<()> {
        let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

        let transaction_request =
            game.challenge().value(self.challenger_bond).into_transaction_request();

        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        tracing::info!(
            "Successfully challenged game {:?} with tx {:?}",
            game_address,
            receipt.transaction_hash
        );

        Ok(())
    }

    /// Gets the oldest valid game address for malicious challenging (for defense mechanisms
    /// testing purposes). This finds games with correct output roots that can be challenged to
    /// test defense mechanisms.
    async fn get_oldest_valid_game_for_malicious_challenge(&self) -> Result<Option<Address>> {
        use fault_proof::contract::ProposalStatus;

        self.factory
            .get_oldest_game_address(
                self.config.max_games_to_check_for_challenge,
                self.l2_provider.clone(),
                |status| status == ProposalStatus::Unchallenged,
                |output_root, game_claim| output_root == game_claim, /* Valid games (opposite of
                                                                      * honest challenger) */
                "Oldest valid game for malicious challenge",
            )
            .await
    }

    /// Handles challenging of invalid games by scanning recent games for potential challenges.
    /// Also supports malicious challenging of valid games for testing defense mechanisms when
    /// configured.
    async fn handle_game_challenging(&self) -> Result<Action> {
        let _span = tracing::info_span!("[[Challenging]]").entered();

        // Challenge invalid games (honest challenger behavior)
        if let Some(game_address) = self
            .factory
            .get_oldest_challengable_game_address(
                self.config.max_games_to_check_for_challenge,
                self.l2_provider.clone(),
            )
            .await?
        {
            tracing::info!(
                "\x1b[32m[CHALLENGE]\x1b[0m Attempting to challenge invalid game {:?}",
                game_address
            );
            self.challenge_game(game_address).await?;
            return Ok(Action::Performed);
        }

        // Maliciously challenge valid games (if configured for testing defense mechanisms)
        if self.config.malicious_challenge_percentage > 0.0 {
            tracing::debug!("Checking for valid games to challenge maliciously...");
            if let Some(game_address) = self.get_oldest_valid_game_for_malicious_challenge().await?
            {
                let mut rng = rand::rng();
                let should_challenge =
                    rng.random_range(0.0..100.0) <= self.config.malicious_challenge_percentage;

                if should_challenge {
                    tracing::warn!(
                        "\x1b[31m[MALICIOUS CHALLENGE]\x1b[0m Attempting to challenge valid game {:?} for testing ({}% chance)",
                        game_address,
                        self.config.malicious_challenge_percentage
                    );
                    self.challenge_game(game_address).await?;
                    return Ok(Action::Performed);
                } else {
                    tracing::debug!(
                        "Found valid game {:?} but skipping malicious challenge ({}% chance)",
                        game_address,
                        self.config.malicious_challenge_percentage
                    );
                }
            } else {
                tracing::debug!("No valid games found for malicious challenging");
            }
        }

        Ok(Action::Skipped)
    }

    /// Handles resolution of challenged games that are ready to be resolved.
    async fn handle_game_resolution(&self) -> Result<()> {
        let _span = tracing::info_span!("[[Resolving]]").entered();

        self.factory
            .resolve_games(
                Mode::Challenger,
                self.config.max_games_to_check_for_resolution,
                self.signer.clone(),
                self.config.l1_rpc.clone(),
                self.l1_provider.clone(),
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
            let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

            // Create a transaction to claim credit
            let transaction_request =
                game.claimCredit(self.challenger_address).into_transaction_request();

            match self
                .signer
                .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
                .await
            {
                Ok(receipt) => {
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

    /// Runs the challenger in an infinite loop, periodically checking for games to challenge and
    /// resolve.
    async fn run(&mut self) -> Result<()> {
        tracing::info!("OP Succinct Challenger running...");
        if self.config.malicious_challenge_percentage > 0.0 {
            tracing::warn!(
                "\x1b[33mMalicious challenging enabled: {}% of valid games will be challenged for testing\x1b[0m",
                self.config.malicious_challenge_percentage
            );
        } else {
            tracing::info!("Honest challenger mode (malicious challenging disabled)");
        }
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        // Each loop, check the oldest challengeable game and challenge it if it exists.
        // Eventually, all games will be challenged (as long as the rate at which games are being
        // created is slower than the fetch interval).
        loop {
            interval.tick().await;

            match self.handle_game_challenging().await {
                Ok(Action::Performed) => {
                    ChallengerGauge::GamesChallenged.increment(1.0);
                }
                Ok(Action::Skipped) => {}
                Err(e) => {
                    tracing::warn!("Failed to handle game challenging: {:?}", e);
                    ChallengerGauge::GameChallengingError.increment(1.0);
                }
            }

            if let Err(e) = self.handle_game_resolution().await {
                tracing::warn!("Failed to handle game resolution: {:?}", e);
                ChallengerGauge::GameResolutionError.increment(1.0);
            }

            match self.handle_bond_claiming().await {
                Ok(Action::Performed) => {
                    ChallengerGauge::GamesBondsClaimed.increment(1.0);
                }
                Ok(Action::Skipped) => {}
                Err(e) => {
                    tracing::warn!("Failed to handle bond claiming: {:?}", e);
                    ChallengerGauge::BondClaimingError.increment(1.0);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logger();

    let args = Args::parse();
    dotenv::from_filename(args.env_file).ok();

    let challenger_signer = Signer::from_env()?;

    let l1_provider = ProviderBuilder::default()
        .connect_http(env::var("L1_RPC").unwrap().parse::<Url>().unwrap());

    let factory = DisputeGameFactory::new(
        env::var("FACTORY_ADDRESS")
            .expect("FACTORY_ADDRESS must be set")
            .parse::<Address>()
            .unwrap(),
        l1_provider.clone(),
    );

    let mut challenger = OPSuccinctChallenger::new(
        challenger_signer.address(),
        challenger_signer,
        l1_provider,
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
