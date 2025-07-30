use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::Result;
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::time;

use crate::{
    config::ChallengerConfig,
    contract::{DisputeGameFactory::DisputeGameFactoryInstance, OPSuccinctFaultDisputeGame},
    prometheus::ChallengerGauge,
    Action, FactoryTrait, L1Provider, L2Provider, Mode,
};
use op_succinct_host_utils::metrics::MetricsGauge;
use op_succinct_signer_utils::Signer;

pub struct OPSuccinctChallenger<P>
where
    P: Provider + Clone,
{
    pub config: ChallengerConfig,
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
    pub async fn from_env(
        l1_provider: L1Provider,
        factory: DisputeGameFactoryInstance<P>,
        signer: Signer,
    ) -> Result<Self> {
        let config = ChallengerConfig::from_env()?;

        Self::new_with_config(config, l1_provider, factory, signer).await
    }

    /// Creates a new challenger instance for testing with provided configuration.
    pub async fn new_with_config(
        config: ChallengerConfig,
        l1_provider: L1Provider,
        factory: DisputeGameFactoryInstance<P>,
        signer: Signer,
    ) -> Result<Self> {
        let challenger_address = signer.address();
        let challenger_bond = factory.fetch_challenger_bond(config.game_type).await?;
        let l2_rpc = config.l2_rpc.clone();

        Ok(OPSuccinctChallenger {
            config,
            challenger_address,
            signer,
            l1_provider: l1_provider.clone(),
            l2_provider: ProviderBuilder::default().connect_http(l2_rpc),
            factory,
            challenger_bond,
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
        use crate::contract::ProposalStatus;

        self.factory
            .get_oldest_game_address(
                self.config.max_games_to_check_for_challenge,
                self.l1_provider.clone(),
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
    #[tracing::instrument(skip(self), level = "info", name = "[[Challenging]]")]
    async fn handle_game_challenging(&self) -> Result<Action> {
        // Challenge invalid games (honest challenger behavior)
        if let Some(game_address) = self
            .factory
            .get_oldest_challengable_game_address(
                self.config.max_games_to_check_for_challenge,
                self.l1_provider.clone(),
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
                let mut rng = StdRng::from_os_rng();
                let should_challenge: f64 = rng.random_range(0.0..100.0);
                let should_challenge =
                    should_challenge <= self.config.malicious_challenge_percentage;

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
    #[tracing::instrument(skip(self), level = "info", name = "[[Resolving]]")]
    async fn handle_game_resolution(&self) -> Result<()> {
        self.factory
            .resolve_games(
                Mode::Challenger,
                self.config.max_games_to_check_for_resolution,
                self.signer.clone(),
                self.config.l1_rpc.clone(),
                self.l1_provider.clone(),
            )
            .await
    }

    /// Handles claiming bonds from resolved games.
    #[tracing::instrument(skip(self), level = "info", name = "[[Claiming Challenger Bonds]]")]
    pub async fn handle_bond_claiming(&self) -> Result<Action> {
        if let Some(game_address) = self
            .factory
            .get_oldest_claimable_bond_game_address(
                self.config.game_type,
                self.config.max_games_to_check_for_bond_claiming,
                self.challenger_address,
                Mode::Challenger,
            )
            .await?
        {
            tracing::info!(
                "Attempting to claim bond from game {:?} where challenger won",
                game_address
            );

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
                        "\x1b[1mSuccessfully claimed challenger bond from game {:?} with tx {:?}\x1b[0m",
                        game_address,
                        receipt.transaction_hash
                    );

                    Ok(Action::Performed)
                }
                Err(e) => Err(anyhow::anyhow!(
                    "Failed to claim challenger bond from game {:?}: {:?}",
                    game_address,
                    e
                )),
            }
        } else {
            tracing::info!("No games found where challenger won to claim bonds from");

            Ok(Action::Skipped)
        }
    }

    /// Runs the challenger in an infinite loop, periodically checking for games to challenge and
    /// resolve.
    pub async fn run(&mut self) -> Result<()> {
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
