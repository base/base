use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::{sync::Mutex, time};

use crate::{
    config::ChallengerConfig,
    contract::{
        DisputeGameFactory::DisputeGameFactoryInstance, GameStatus, OPSuccinctFaultDisputeGame,
        ProposalStatus,
    },
    is_parent_challenger_wins, is_parent_resolved,
    prometheus::ChallengerGauge,
    FactoryTrait, L1Provider, L2Provider, L2ProviderTrait,
};
use op_succinct_host_utils::metrics::MetricsGauge;
use op_succinct_signer_utils::SignerLock;

pub struct OPSuccinctChallenger<P>
where
    P: Provider + Clone,
{
    pub config: ChallengerConfig,
    signer: SignerLock,
    l1_provider: L1Provider,
    l2_provider: L2Provider,
    factory: DisputeGameFactoryInstance<P>,
    challenger_bond: U256,
    state: Arc<Mutex<ChallengerState>>,
}

impl<P> OPSuccinctChallenger<P>
where
    P: Provider + Clone,
{
    /// Creates a new challenger instance for testing with provided configuration.
    pub async fn new(
        config: ChallengerConfig,
        l1_provider: L1Provider,
        factory: DisputeGameFactoryInstance<P>,
        signer: SignerLock,
    ) -> Result<Self> {
        let challenger_bond = factory.fetch_challenger_bond(config.game_type).await?;
        let l2_rpc = config.l2_rpc.clone();

        Ok(OPSuccinctChallenger {
            config,
            signer,
            l1_provider: l1_provider.clone(),
            l2_provider: ProviderBuilder::default().connect_http(l2_rpc),
            factory,
            challenger_bond,
            state: Arc::new(Mutex::new(ChallengerState {
                cursor: U256::ZERO,
                games: HashMap::new(),
            })),
        })
    }

    /// Runs the main challenger loop. On each tick it waits for the configured interval, refreshes
    /// cached state, and then handles challenging, resolution, and bond-claiming tasks.
    pub async fn run(&mut self) -> Result<()> {
        tracing::info!("OP Succinct Lite Challenger running...");
        if self.config.malicious_challenge_percentage > 0.0 {
            tracing::warn!(
                "\x1b[33mMalicious challenging enabled: {}% of valid games will be challenged for testing\x1b[0m",
                self.config.malicious_challenge_percentage
            );
        } else {
            tracing::info!("Honest challenger mode (malicious challenging disabled)");
        }

        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        // Each loop iteration waits for the configured interval, synchronizes the cached state,
        // and then attempts to challenge, resolve, and claim bonds for any eligible games.
        loop {
            interval.tick().await;

            // Synchronize cached dispute state before scheduling work.
            if let Err(e) = self.sync_state().await {
                tracing::warn!("Failed to sync challenger state: {:?}", e);
            }

            if let Err(e) = self.handle_game_challenging().await {
                tracing::warn!("Failed to handle game challenging: {:?}", e);
            }

            if let Err(e) = self.handle_game_resolution().await {
                tracing::warn!("Failed to handle game resolution: {:?}", e);
            }

            if let Err(e) = self.handle_bond_claiming().await {
                tracing::warn!("Failed to handle bond claiming: {:?}", e);
            }
        }
    }

    /// Synchronizes the game cache.
    ///
    /// 1. Load new games.
    ///    - Incrementally load new games from the factory starting from the cursor.
    /// 2. Synchronize the status of all cached games.
    ///    - Games are marked for challenging if output root is invalid or the parent is challenger
    ///      wins.
    ///    - Games are marked for resolution if the parent is resolved, the game is over, and it's
    ///      own game.
    ///    - Games are marked for bond claim if they are finalized and there is credit to claim.
    ///    - Games are evicted once finalized with no remaining credit or whenever resolves as
    ///      defender wins.
    async fn sync_state(&self) -> Result<()> {
        // 1. Load new games.
        let mut next_index = {
            let state = self.state.lock().await;
            // If cursor is 0, start from 0; otherwise, start from cursor + 1
            if state.cursor == U256::ZERO && state.games.is_empty() {
                U256::ZERO
            } else {
                state.cursor + U256::from(1)
            }
        };

        let Some(latest_index) = self.factory.fetch_latest_game_index().await? else {
            return Ok(());
        };

        while next_index <= latest_index {
            self.fetch_game(next_index).await?;
            next_index += U256::from(1);
        }

        // 2. Synchronize the status of all cached games.
        let games = {
            let state = self.state.lock().await;
            state.games.values().cloned().collect::<Vec<_>>()
        };

        if !games.is_empty() {
            let now_ts = self
                .l1_provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .context("Failed to fetch latest L1 block timestamp")?
                .header
                .timestamp;
            let signer_address = self.signer.address();

            enum GameSyncAction {
                Update {
                    index: U256,
                    status: GameStatus,
                    proposal_status: ProposalStatus,
                    should_attempt_to_challenge: bool,
                    should_attempt_to_resolve: bool,
                    should_attempt_to_claim_bond: bool,
                },
                Remove(U256),
            }

            let mut actions = Vec::with_capacity(games.len());

            for game in games {
                let contract =
                    OPSuccinctFaultDisputeGame::new(game.address, self.l1_provider.clone());
                let status = contract.status().call().await?;
                let claim_data = contract.claimData().call().await?;
                let proposal_status = claim_data.status;
                let deadline = U256::from(claim_data.deadline).to::<u64>();

                match status {
                    GameStatus::IN_PROGRESS => {
                        let is_game_over = match proposal_status {
                            ProposalStatus::Challenged => now_ts >= deadline,
                            _ => false,
                        };

                        if proposal_status == ProposalStatus::Unchallenged {
                            let is_parent_challenger_wins =
                                is_parent_challenger_wins(game.parent_index, &self.factory).await?;

                            if !is_game_over && (game.is_invalid || is_parent_challenger_wins) {
                                actions.push(GameSyncAction::Update {
                                    index: game.index,
                                    status,
                                    proposal_status,
                                    should_attempt_to_challenge: true,
                                    should_attempt_to_resolve: false,
                                    should_attempt_to_claim_bond: false,
                                });
                            }
                        } else if proposal_status == ProposalStatus::Challenged {
                            let is_parent_resolved =
                                is_parent_resolved(game.parent_index, &self.factory).await?;
                            let is_own_game = claim_data.counteredBy == signer_address;

                            if is_game_over && is_parent_resolved && is_own_game {
                                actions.push(GameSyncAction::Update {
                                    index: game.index,
                                    status,
                                    proposal_status,
                                    should_attempt_to_challenge: false,
                                    should_attempt_to_resolve: true,
                                    should_attempt_to_claim_bond: false,
                                });
                            }
                        }
                    }
                    GameStatus::CHALLENGER_WINS => {
                        let is_finalized = self
                            .factory
                            .is_game_finalized(self.config.game_type, game.address)
                            .await?;
                        let credit = contract.credit(signer_address).call().await?;

                        if is_finalized && credit == U256::ZERO {
                            actions.push(GameSyncAction::Remove(game.index));
                        } else {
                            actions.push(GameSyncAction::Update {
                                index: game.index,
                                status,
                                proposal_status,
                                should_attempt_to_challenge: false,
                                should_attempt_to_resolve: false,
                                should_attempt_to_claim_bond: is_finalized && credit > U256::ZERO,
                            });
                        }
                    }
                    GameStatus::DEFENDER_WINS => {
                        actions.push(GameSyncAction::Remove(game.index));
                    }
                    _ => unreachable!("Unexpected game status: {:?}", status),
                }
            }

            let mut state = self.state.lock().await;
            for action in actions {
                match action {
                    GameSyncAction::Update {
                        index,
                        status,
                        proposal_status,
                        should_attempt_to_challenge,
                        should_attempt_to_resolve,
                        should_attempt_to_claim_bond,
                    } => {
                        if let Some(game) = state.games.get_mut(&index) {
                            game.status = status;
                            game.proposal_status = proposal_status;
                            game.should_attempt_to_challenge = should_attempt_to_challenge;
                            game.should_attempt_to_resolve = should_attempt_to_resolve;
                            game.should_attempt_to_claim_bond = should_attempt_to_claim_bond;
                        }
                    }
                    GameSyncAction::Remove(index) => {
                        state.games.remove(&index);
                    }
                }
            }
        }

        Ok(())
    }

    /// Fetch game from the factory.
    ///
    /// Drop game if the game type is invalid or the game was not respected at the time of creation.
    async fn fetch_game(&self, index: U256) -> Result<()> {
        let game = self.factory.gameAtIndex(index).call().await?;
        let game_address = game.proxy;
        let contract = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

        let game_type = contract.gameType().call().await?;
        if game_type != self.config.game_type {
            tracing::debug!(game_index = %index, ?game_address, game_type,
                expected_game_type = self.config.game_type,
                "Dropping game due to invalid game type"
            );
            return Ok(());
        }

        let l2_block_number = contract.l2BlockNumber().call().await?;
        let computed_output_root =
            self.l2_provider.compute_output_root_at_block(l2_block_number).await?;
        let output_root = contract.rootClaim().call().await?;
        let claim_data = contract.claimData().call().await?;

        let was_respected = contract.wasRespectedGameTypeWhenCreated().call().await?;
        let status = contract.status().call().await?;

        let mut state = self.state.lock().await;

        if was_respected {
            state.games.insert(
                index,
                Game {
                    index,
                    address: game_address,
                    parent_index: claim_data.parentIndex,
                    l2_block_number,
                    is_invalid: output_root != computed_output_root,
                    status,
                    proposal_status: claim_data.status,
                    should_attempt_to_challenge: false,
                    should_attempt_to_resolve: false,
                    should_attempt_to_claim_bond: false,
                },
            );
        } else {
            tracing::debug!(
                game_index = %index,
                ?game_address,
                game_type,
                expected_game_type = self.config.game_type,
                "Dropping game because its type was not respected at the time of creation"
            );
        }

        state.cursor = index;

        Ok(())
    }

    /// Challenges games flagged for challenging.
    /// Also supports malicious challenging of valid games for testing defense mechanisms when
    /// configured.
    #[tracing::instrument(skip(self), level = "info", name = "[[Challenging]]")]
    async fn handle_game_challenging(&mut self) -> Result<()> {
        let candidates = {
            let state = self.state.lock().await;
            state
                .games
                .values()
                .filter(|game| game.should_attempt_to_challenge)
                .cloned()
                .collect::<Vec<_>>()
        };

        for game in candidates {
            if let Err(error) = self.submit_challenge_transaction(&game).await {
                tracing::warn!(
                    game_index = %game.index,
                    game_address = ?game.address,
                    ?error,
                    "Failed to challenge game"
                );
                ChallengerGauge::GameChallengingError.increment(1.0);
                continue;
            }

            // Clear the challenge flag after successful challenge
            {
                let mut state = self.state.lock().await;
                if let Some(game_state) = state.games.get_mut(&game.index) {
                    game_state.should_attempt_to_challenge = false;
                }
            }

            ChallengerGauge::GamesChallenged.increment(1.0);
        }

        // Maliciously challenge valid games (if configured for testing defense mechanisms)
        if self.config.malicious_challenge_percentage > 0.0 {
            let mut rng = StdRng::from_os_rng();
            let should_challenge: f64 = rng.random_range(0.0..100.0);

            if should_challenge <= self.config.malicious_challenge_percentage {
                let candidate = {
                    let state = self.state.lock().await;
                    state
                        .games
                        .values()
                        .filter(|game| !game.should_attempt_to_challenge)
                        .min_by_key(|game| game.index)
                        .cloned()
                };

                if let Some(game) = candidate {
                    tracing::warn!(
                        "\x1b[31m[MALICIOUS CHALLENGE]\x1b[0m Attempting to challenge valid game {:?} at index {} for testing ({}% chance)",
                        game.address,
                        game.index,
                        self.config.malicious_challenge_percentage
                    );

                    if let Err(error) = self.submit_challenge_transaction(&game).await {
                        tracing::warn!(
                            game_index = %game.index,
                            game_address = ?game.address,
                            ?error,
                            "Failed to maliciously challenge game"
                        );
                        ChallengerGauge::GameChallengingError.increment(1.0);
                    } else {
                        // Clear the challenge flag after successful malicious challenge
                        {
                            let mut state = self.state.lock().await;
                            if let Some(game_state) = state.games.get_mut(&game.index) {
                                game_state.should_attempt_to_challenge = false;
                            }
                        }
                        ChallengerGauge::GamesChallenged.increment(1.0);
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn submit_challenge_transaction(&self, game: &Game) -> Result<()> {
        let contract = OPSuccinctFaultDisputeGame::new(game.address, self.l1_provider.clone());
        let transaction_request =
            contract.challenge().value(self.challenger_bond).into_transaction_request();
        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        tracing::info!(
            game_index = %game.index,
            game_address = ?game.address,
            l2_block = %game.l2_block_number,
            tx_hash = ?receipt.transaction_hash,
            "Game challenged successfully"
        );

        Ok(())
    }

    /// Resolves games flagged for resolution.
    #[tracing::instrument(skip(self), level = "info", name = "[[Resolving]]")]
    async fn handle_game_resolution(&self) -> Result<()> {
        let candidates = {
            let state = self.state.lock().await;
            state
                .games
                .values()
                .filter(|game| game.should_attempt_to_resolve)
                .cloned()
                .collect::<Vec<_>>()
        };

        for game in candidates {
            if let Err(error) = self.submit_resolution_transaction(&game).await {
                tracing::warn!(
                    game_index = %game.index,
                    game_address = ?game.address,
                    ?error,
                    "Failed to resolve game"
                );
                ChallengerGauge::GameResolutionError.increment(1.0);
                continue;
            }

            ChallengerGauge::GamesResolved.increment(1.0);
        }

        Ok(())
    }

    pub async fn submit_resolution_transaction(&self, game: &Game) -> Result<()> {
        let contract = OPSuccinctFaultDisputeGame::new(game.address, self.l1_provider.clone());
        let transaction_request = contract.resolve().into_transaction_request();
        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        tracing::info!(
            game_index = %game.index,
            game_address = ?game.address,
            l2_block_end = %game.l2_block_number,
            tx_hash = ?receipt.transaction_hash,
            "Game resolved successfully"
        );

        Ok(())
    }

    /// Claims bonds from games flagged for claiming.
    #[tracing::instrument(skip(self), level = "info", name = "[[Claiming Challenger Bonds]]")]
    pub async fn handle_bond_claiming(&self) -> Result<()> {
        let candidates = {
            let state = self.state.lock().await;
            state
                .games
                .values()
                .filter(|game| game.should_attempt_to_claim_bond)
                .cloned()
                .collect::<Vec<_>>()
        };

        for game in candidates {
            if let Err(error) = self.submit_bond_claim_transaction(&game).await {
                tracing::warn!(
                    game_index = %game.index,
                    game_address = ?game.address,
                    ?error,
                    "Failed to claim bond for game"
                );
                ChallengerGauge::BondClaimingError.increment(1.0);
                continue;
            }

            ChallengerGauge::GamesBondsClaimed.increment(1.0);
        }

        Ok(())
    }

    #[tracing::instrument(name = "[[Claiming Proposer Bonds]]", skip(self, game))]
    async fn submit_bond_claim_transaction(&self, game: &Game) -> Result<()> {
        let contract = OPSuccinctFaultDisputeGame::new(game.address, self.l1_provider.clone());
        let transaction_request =
            contract.claimCredit(self.signer.address()).gas(200_000).into_transaction_request();
        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        tracing::info!(
            game_index = %game.index,
            game_address = ?game.address,
            l2_block_end = %game.l2_block_number,
            tx_hash = ?receipt.transaction_hash,
            "Bond claimed successfully"
        );

        Ok(())
    }
}

#[derive(Clone)]
pub struct Game {
    pub index: U256,
    pub address: Address,
    pub parent_index: u32,
    pub l2_block_number: U256,
    pub is_invalid: bool,
    pub status: GameStatus,
    pub proposal_status: ProposalStatus,
    pub should_attempt_to_challenge: bool,
    pub should_attempt_to_resolve: bool,
    pub should_attempt_to_claim_bond: bool,
}

pub struct ChallengerState {
    cursor: U256,
    games: HashMap<U256, Game>,
}
