use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, FixedBytes, TxHash, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_sol_types::{SolEvent, SolValue};
use anyhow::{Context, Result};
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher,
    get_agg_proof_stdin,
    host::OPSuccinctHost,
    metrics::MetricsGauge,
    network::{determine_network_mode, get_network_signer},
    witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use op_succinct_signer_utils::SignerLock;
use sp1_sdk::{
    NetworkProver, Prover, ProverClient, SP1ProofMode, SP1ProofWithPublicValues, SP1ProvingKey,
    SP1VerifyingKey, SP1_CIRCUIT_VERSION,
};
use tokio::{sync::Mutex, time};

use crate::{
    config::ProposerConfig,
    contract::{
        DisputeGameFactory::{DisputeGameCreated, DisputeGameFactoryInstance},
        GameStatus, OPSuccinctFaultDisputeGame, ProposalStatus,
    },
    is_parent_resolved,
    prometheus::ProposerGauge,
    FactoryTrait, L1Provider, L2Provider, L2ProviderTrait,
};

/// Type alias for task ID
pub type TaskId = u64;

/// Type alias for task handles
pub type TaskHandle = tokio::task::JoinHandle<Result<()>>;

/// Type alias for a map of task IDs to their join handles and associated task info
pub type TaskMap = HashMap<TaskId, (TaskHandle, TaskInfo)>;

/// Information about a running task
#[derive(Clone, Debug)]
pub enum TaskInfo {
    GameCreation { block_number: U256 },
    GameProving { game_address: Address, is_defense: bool },
    GameResolution,
    BondClaim,
}

#[derive(Clone)]
struct SP1Prover {
    network_prover: Arc<NetworkProver>,
    range_pk: Arc<SP1ProvingKey>,
    range_vk: Arc<SP1VerifyingKey>,
    agg_pk: Arc<SP1ProvingKey>,
    agg_mode: SP1ProofMode,
}

/// Represents a dispute game in the on-chain game DAG.
///
/// Games form a directed acyclic graph where each game builds upon a parent game, extending the
/// chain with a new proposed output root. The proposer tracks these games to determine when to
/// propose new games, defend existing ones, resolve completed games and claim bonds.
#[derive(Clone)]
pub struct Game {
    pub index: U256,
    pub address: Address,
    pub parent_index: u32,
    pub l2_block: U256,
    pub status: GameStatus,
    pub proposal_status: ProposalStatus,
    pub deadline: u64,
    pub should_attempt_to_resolve: bool,
    pub should_attempt_to_claim_bond: bool,
}

/// Central cache of the proposer's view of dispute games.
///
/// Tracks:
/// - `anchor_game`: the latest anchor fetched from the registry
/// - `canonical_head_index`/`canonical_head_l2_block`: the best known game for scheduling work
/// - `cursor`: the last index of factory's dispute game list processed during incremental syncs
/// - `games`: cached metadata for every tracked game keyed by index
#[derive(Default)]
struct ProposerState {
    anchor_game: Option<Game>,
    canonical_head_index: Option<U256>,
    canonical_head_l2_block: Option<U256>,
    cursor: Option<U256>,
    games: HashMap<U256, Game>,
}

impl ProposerState {
    /// Returns all game indices reachable from `root_index`, including the root.
    ///
    /// NOTE(fakedev9999): If this becomes hot, consider maintaining an adjacency index
    /// `parent -> Vec<child>`.
    fn descendants_of(&self, root_index: U256) -> HashSet<U256> {
        let mut reachable: HashSet<U256> = HashSet::new();
        let mut stack = vec![root_index];

        while let Some(index) = stack.pop() {
            if reachable.insert(index) {
                stack.extend(
                    self.games
                        .values()
                        .filter(|game| U256::from(game.parent_index) == index)
                        .map(|game| game.index),
                );
            }
        }

        reachable
    }

    /// Remove a game subtree from the cache.
    ///
    /// Used when a game is invalidated (i.e., `CHALLENGER_WINS`) and its entire subtree must be
    /// dropped.
    fn remove_subtree(&mut self, root_index: U256) {
        tracing::info!(?root_index, "Removing subtree from cache");
        for index in self.descendants_of(root_index) {
            tracing::info!(?index, "Removing game from cache");
            self.games.remove(&index);
        }
    }
}

#[derive(Clone)]
pub struct OPSuccinctProposer<P, H: OPSuccinctHost>
where
    P: Provider + Clone + Send + Sync + 'static,
    H: OPSuccinctHost + Clone + Send + Sync + 'static,
{
    pub config: ProposerConfig,
    pub signer: SignerLock,
    pub l1_provider: L1Provider,
    pub l2_provider: L2Provider,
    pub factory: Arc<DisputeGameFactoryInstance<P>>,
    pub init_bond: U256,
    pub safe_db_fallback: bool,
    prover: SP1Prover,
    fetcher: Arc<OPSuccinctDataFetcher>,
    host: Arc<H>,
    tasks: Arc<Mutex<TaskMap>>,
    next_task_id: Arc<AtomicU64>,
    state: Arc<Mutex<ProposerState>>,
}

impl<P, H> OPSuccinctProposer<P, H>
where
    P: Provider + Clone + Send + Sync + 'static,
    H: OPSuccinctHost + Clone + Send + Sync + 'static,
{
    /// Creates a new proposer instance with the provided L1 provider with wallet and factory
    /// contract instance.
    pub async fn new(
        config: ProposerConfig,
        signer: SignerLock,
        factory: DisputeGameFactoryInstance<P>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        host: Arc<H>,
    ) -> Result<Self> {
        // Set up the network prover.
        let network_signer = get_network_signer(config.use_kms_requester).await?;
        let network_mode =
            determine_network_mode(config.range_proof_strategy, config.agg_proof_strategy)?;
        let network_prover = Arc::new(
            ProverClient::builder().network_for(network_mode).signer(network_signer).build(),
        );
        let (range_pk, range_vk) = network_prover.setup(get_range_elf_embedded());
        let (agg_pk, _) = network_prover.setup(AGGREGATION_ELF);

        let l1_provider = ProviderBuilder::default().connect_http(config.l1_rpc.clone());
        let l2_provider = ProviderBuilder::default().connect_http(config.l2_rpc.clone());
        let init_bond = factory.fetch_init_bond(config.game_type).await?;

        // Initialize state with anchor L2 block number
        let anchor_l2_block = factory.get_anchor_l2_block_number(config.game_type).await?;
        let initial_state =
            ProposerState { canonical_head_l2_block: Some(anchor_l2_block), ..Default::default() };

        Ok(Self {
            config: config.clone(),
            signer,
            l1_provider,
            l2_provider,
            factory: Arc::new(factory.clone()),
            init_bond,
            safe_db_fallback: config.safe_db_fallback,
            prover: SP1Prover {
                network_prover,
                range_pk: Arc::new(range_pk),
                range_vk: Arc::new(range_vk),
                agg_pk: Arc::new(agg_pk),
                agg_mode: config.agg_proof_mode,
            },
            fetcher: fetcher.clone(),
            host,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_task_id: Arc::new(AtomicU64::new(1)),
            state: Arc::new(Mutex::new(initial_state)),
        })
    }

    /// Runs the proposer indefinitely.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tracing::info!("OP Succinct Proposer running...");
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        // Spawn a dedicated task for continuous metrics collection
        self.spawn_metrics_collector();

        loop {
            interval.tick().await;

            // 1. Synchronize cached dispute state before scheduling work.
            if let Err(e) = self.sync_state().await {
                tracing::warn!("Failed to sync proposer state: {:?}", e);
            }

            // 2. Handle completed tasks.
            if let Err(e) = self.handle_completed_tasks().await {
                tracing::warn!("Failed to handle completed tasks: {:?}", e);
            }

            // 3. Spawn new work (non-blocking).
            if let Err(e) = self.spawn_pending_operations().await {
                tracing::warn!("Failed to spawn pending operations: {:?}", e);
            }

            // 4. Log task statistics.
            self.log_task_stats().await;
        }
    }

    /// Synchronizes the proposer's cached view of the dispute-game tree with the on-chain state.
    ///
    /// Steps run in order:
    /// 1. `sync_games` pulls newly created games and refreshes cached metadata.
    /// 2. `sync_anchor_game` aligns the cached anchor pointer with the registry contract.
    /// 3. `compute_canonical_head` recomputes the head game used for proposal selection.
    async fn sync_state(&self) -> Result<()> {
        // Pull new games and synchronize cached game statuses.
        self.sync_games().await?;

        // Align anchor information after the cached game statuses have been synchronized.
        self.sync_anchor_game().await?;

        // With the cached game statuses and anchor synchronized, recompute the canonical head.
        self.compute_canonical_head().await;

        Ok(())
    }

    /// Synchronizes the game cache.
    ///
    /// 1. Load new games.
    ///    - Incrementally load new games from the factory starting from the cursor.
    ///    - Games are validated (correct type, valid output root) before being added.
    /// 2. Synchronize the status of all cached games.
    ///    - Games are marked for resolution if the parent is resolved, the game is over, and it's
    ///      own game.
    ///    - Games are marked for bond claim if they are finalized and there is credit to claim.
    /// 3. Evict games from the cache.
    ///    - Games that are finalized but there is no credit left to claim.
    ///    - The entire subtree of a CHALLENGER_WINS game.
    async fn sync_games(&self) -> Result<()> {
        // 1. Load new games.
        let mut next_index = {
            let state = self.state.lock().await;
            match state.cursor {
                Some(cursor) => cursor + U256::from(1),
                None => U256::ZERO,
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
            state.games.values().map(|game| (game.index, game.address)).collect::<Vec<_>>()
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
                    deadline: u64,
                    should_attempt_to_resolve: bool,
                    should_attempt_to_claim_bond: bool,
                },
                Remove(U256),
                RemoveSubtree(U256),
            }

            let mut actions = Vec::with_capacity(games.len());

            for (index, game_address) in games {
                let contract =
                    OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
                let claim_data = contract.claimData().call().await?;
                let status = contract.status().call().await?;
                let deadline = U256::from(claim_data.deadline).to::<u64>();
                let parent_index = claim_data.parentIndex;
                let is_finalized =
                    self.factory.is_game_finalized(self.config.game_type, game_address).await?;

                match status {
                    GameStatus::IN_PROGRESS => {
                        let game_type = contract.gameType().call().await?;
                        let parent_resolved =
                            is_parent_resolved(parent_index, self.factory.as_ref()).await?;
                        let is_game_over = match claim_data.status {
                            ProposalStatus::Unchallenged => now_ts >= deadline,
                            ProposalStatus::UnchallengedAndValidProofProvided |
                            ProposalStatus::ChallengedAndValidProofProvided => true,
                            _ => false,
                        };
                        let creator = contract.gameCreator().call().await?;
                        let is_own_game = match claim_data.status {
                            ProposalStatus::Unchallenged => creator == signer_address,
                            ProposalStatus::UnchallengedAndValidProofProvided |
                            ProposalStatus::ChallengedAndValidProofProvided => {
                                creator == signer_address || claim_data.prover == signer_address
                            }
                            _ => false,
                        };

                        let should_attempt_to_resolve = game_type == self.config.game_type &&
                            parent_resolved &&
                            is_game_over &&
                            is_own_game;

                        actions.push(GameSyncAction::Update {
                            index,
                            status,
                            proposal_status: claim_data.status,
                            deadline,
                            should_attempt_to_resolve,
                            should_attempt_to_claim_bond: false,
                        });
                    }
                    GameStatus::DEFENDER_WINS => {
                        let credit = contract.credit(signer_address).call().await?;

                        if is_finalized && credit == U256::ZERO {
                            actions.push(GameSyncAction::Remove(index));
                        } else {
                            actions.push(GameSyncAction::Update {
                                index,
                                status,
                                proposal_status: claim_data.status,
                                deadline,
                                should_attempt_to_resolve: false,
                                should_attempt_to_claim_bond: is_finalized && credit > U256::ZERO,
                            });
                        }
                    }
                    GameStatus::CHALLENGER_WINS => {
                        actions.push(GameSyncAction::RemoveSubtree(index));
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
                        deadline,
                        should_attempt_to_resolve,
                        should_attempt_to_claim_bond,
                    } => {
                        if let Some(game) = state.games.get_mut(&index) {
                            game.status = status;
                            game.proposal_status = proposal_status;
                            game.deadline = deadline;
                            game.should_attempt_to_resolve = should_attempt_to_resolve;
                            game.should_attempt_to_claim_bond = should_attempt_to_claim_bond;
                        }
                    }
                    GameSyncAction::Remove(index) => {
                        let is_canonical_head = state.canonical_head_index == Some(index);

                        if is_canonical_head {
                            tracing::debug!(
                                game_index = %index,
                                "Retaining canonical head game in cache despite zero credit"
                            );
                        } else {
                            state.games.remove(&index);
                        }
                    }
                    GameSyncAction::RemoveSubtree(index) => {
                        state.remove_subtree(index);
                    }
                }
            }
        }

        Ok(())
    }

    /// Synchronizes the anchor game from the factory.
    async fn sync_anchor_game(&self) -> Result<()> {
        let anchor_game = self.factory.get_anchor_game(self.config.game_type).await?;
        let anchor_address = anchor_game.address();

        if *anchor_address != Address::ZERO {
            let mut state = self.state.lock().await;

            // Fetch the anchor game from the cache.
            if let Some((_, anchor_game)) =
                state.games.iter().find(|(_, game)| game.address == *anchor_address)
            {
                state.anchor_game = Some(anchor_game.clone());
            } else {
                tracing::debug!(?anchor_address, "Anchor game not in cache yet");
            }
        }

        Ok(())
    }

    /// Computes the canonical head by scanning all cached games.
    ///
    /// Canonical head is the game with the highest L2 block number. When an anchor game is present,
    /// only its descendants are eligible for canonical head.
    async fn compute_canonical_head(&self) {
        let mut state = self.state.lock().await;

        let canonical_head = if let Some(anchor_game) = state.anchor_game.as_ref() {
            let reachable = state.descendants_of(anchor_game.index);
            state
                .games
                .values()
                .filter(|game| reachable.contains(&game.index))
                .max_by_key(|game| game.l2_block)
                .cloned()
        } else {
            state.games.values().max_by_key(|game| game.l2_block).cloned()
        };

        let previous_canonical_index = state.canonical_head_index;

        if let Some(canonical_head) = canonical_head {
            state.canonical_head_index = Some(canonical_head.index);
            state.canonical_head_l2_block = Some(canonical_head.l2_block);

            if previous_canonical_index != state.canonical_head_index {
                tracing::info!(
                    previous_canonical_index = ?previous_canonical_index,
                    new_canonical_index = %canonical_head.index,
                    l2_block = %canonical_head.l2_block,
                    total_games = state.games.len(),
                    "Canonical head updated"
                );
            }
        } else {
            // Clear stale canonical head index when no valid games exist.
            state.canonical_head_index = None;

            if previous_canonical_index.is_some() {
                tracing::info!(
                    previous_canonical_index = ?previous_canonical_index,
                    total_games = state.games.len(),
                    "Canonical head cleared: no valid games in cache"
                );
            }
        }
    }

    /// Proves a dispute game at the given address.
    ///
    /// # Returns
    /// A tuple containing:
    /// - `TxHash`: The transaction hash of the proof submission
    /// - `u64`: Total instruction cycles used in the proof generation
    /// - `u64`: Total SP1 gas consumed in the proof generation
    #[tracing::instrument(name = "[[Proving]]", skip(self), fields(game_address = ?game_address))]
    pub async fn prove_game(&self, game_address: Address) -> Result<(TxHash, u64, u64)> {
        tracing::info!("Attempting to prove game {:?}", game_address);

        let fetcher = match OPSuccinctDataFetcher::new_with_rollup_config().await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to create data fetcher: {}", e);
                return Err(anyhow::anyhow!("Failed to create data fetcher: {}", e));
            }
        };

        let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
        let l1_head_hash = game.l1Head().call().await?.0;
        tracing::debug!("L1 head hash: {:?}", hex::encode(l1_head_hash));
        let l2_block_number = game.l2BlockNumber().call().await?;

        let host_args = self
            .host
            .fetch(
                l2_block_number.to::<u64>() - self.config.proposal_interval_in_blocks,
                l2_block_number.to::<u64>(),
                Some(l1_head_hash.into()),
                self.config.safe_db_fallback,
            )
            .await
            .context("Failed to get host CLI args")?;

        let witness_data = self.host.run(&host_args).await?;

        let sp1_stdin = match self.host.witness_generator().get_sp1_stdin(witness_data) {
            Ok(stdin) => stdin,
            Err(e) => {
                tracing::error!("Failed to get proof stdin: {}", e);
                return Err(anyhow::anyhow!("Failed to get proof stdin: {}", e));
            }
        };

        tracing::info!("Generating Range Proof");
        let (range_proof, total_instruction_cycles, total_sp1_gas) = if self.config.mock_mode {
            tracing::info!("Using mock mode for range proof generation");
            let (public_values, report) = self
                .prover
                .network_prover
                .execute(get_range_elf_embedded(), &sp1_stdin)
                .calculate_gas(true)
                .deferred_proof_verification(false)
                .run()?;

            // Record execution stats
            let total_instruction_cycles = report.total_instruction_count();
            let total_sp1_gas = report.gas.unwrap_or(0);

            // Update Prometheus metrics
            ProposerGauge::TotalInstructionCycles.set(total_instruction_cycles as f64);
            ProposerGauge::TotalSP1Gas.set(total_sp1_gas as f64);

            tracing::info!(
                total_instruction_cycles = total_instruction_cycles,
                total_sp1_gas = total_sp1_gas,
                "Captured execution stats for range proof"
            );

            // Create a mock range proof with the public values.
            let proof = SP1ProofWithPublicValues::create_mock_proof(
                &self.prover.range_pk,
                public_values,
                SP1ProofMode::Compressed,
                SP1_CIRCUIT_VERSION,
            );

            (proof, total_instruction_cycles, total_sp1_gas)
        } else {
            // In network mode, we don't have access to execution stats
            let proof = self
                .prover
                .network_prover
                .prove(&self.prover.range_pk, &sp1_stdin)
                .compressed()
                .skip_simulation(true)
                .strategy(self.config.range_proof_strategy)
                .timeout(Duration::from_secs(self.config.timeout))
                .min_auction_period(self.config.min_auction_period)
                .max_price_per_pgu(self.config.max_price_per_pgu)
                .cycle_limit(self.config.range_cycle_limit)
                .gas_limit(self.config.range_gas_limit)
                .whitelist(self.config.whitelist.clone())
                .run_async()
                .await?;

            (proof, 0, 0)
        };

        tracing::info!("Preparing Stdin for Agg Proof");
        let proof = range_proof.proof.clone();
        let mut public_values = range_proof.public_values.clone();
        let boot_info: BootInfoStruct = public_values.read();

        let headers = match fetcher
            .get_header_preimages(&vec![boot_info.clone()], boot_info.clone().l1Head)
            .await
        {
            Ok(headers) => headers,
            Err(e) => {
                tracing::error!("Failed to get header preimages: {}", e);
                return Err(anyhow::anyhow!("Failed to get header preimages: {}", e));
            }
        };

        let sp1_stdin = match get_agg_proof_stdin(
            vec![proof],
            vec![boot_info.clone()],
            headers,
            &self.prover.range_vk,
            boot_info.l1Head,
            self.signer.address(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to get agg proof stdin: {}", e);
                return Err(anyhow::anyhow!("Failed to get agg proof stdin: {}", e));
            }
        };

        tracing::info!("Generating Agg Proof");
        let agg_proof = if self.config.mock_mode {
            tracing::info!("Using mock mode for aggregation proof generation");
            let (public_values, _) = self
                .prover
                .network_prover
                .execute(AGGREGATION_ELF, &sp1_stdin)
                .deferred_proof_verification(false)
                .run()?;

            // Create a mock aggregation proof with the public values.
            SP1ProofWithPublicValues::create_mock_proof(
                &self.prover.agg_pk,
                public_values,
                self.prover.agg_mode,
                SP1_CIRCUIT_VERSION,
            )
        } else {
            self.prover
                .network_prover
                .prove(&self.prover.agg_pk, &sp1_stdin)
                .mode(self.prover.agg_mode)
                .strategy(self.config.agg_proof_strategy)
                .timeout(Duration::from_secs(self.config.timeout))
                .min_auction_period(self.config.min_auction_period)
                .max_price_per_pgu(self.config.max_price_per_pgu)
                .cycle_limit(self.config.agg_cycle_limit)
                .gas_limit(self.config.agg_gas_limit)
                .whitelist(self.config.whitelist.clone())
                .run_async()
                .await?
        };

        let transaction_request = game.prove(agg_proof.bytes().into()).into_transaction_request();

        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        Ok((receipt.transaction_hash, total_instruction_cycles, total_sp1_gas))
    }

    /// Creates a new game with the given parameters.
    ///
    /// `output_root`: the output root we are proposing.
    /// `extra_data`: the extra data of the game; the l2 block number and the parent game index.
    pub async fn create_game(
        &self,
        output_root: FixedBytes<32>,
        extra_data: Vec<u8>,
    ) -> Result<Address> {
        let transaction_request = self
            .factory
            .create(self.config.game_type, output_root, extra_data.into())
            .value(self.init_bond)
            .into_transaction_request();

        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        let game_address = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| {
                DisputeGameCreated::decode_log(&log.inner).ok().map(|event| event.disputeProxy)
            })
            .context("Could not find DisputeGameCreated event in transaction receipt logs")?;

        // Fetch game index after creation
        let game_count = self.factory.gameCount().call().await?;
        let game_index = game_count - U256::from(1);

        tracing::info!(
            game_index = %game_index,
            game_address = ?game_address,
            tx_hash = ?receipt.transaction_hash,
            "Game created successfully"
        );

        if self.config.fast_finality_mode {
            tracing::info!("Fast finality mode enabled: Spawning proof generation task");

            // Spawn a tracked proving task for the new game
            if let Err(e) = self.spawn_game_proving_task(game_address, false).await {
                tracing::warn!("Failed to spawn fast finality proof task: {:?}", e);
            }
        }

        Ok(game_address)
    }

    async fn resolve_games(&self) -> Result<()> {
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
                    l2_block_end = %game.l2_block,
                    ?error,
                    "Failed to resolve game"
                );
                ProposerGauge::GameResolutionError.increment(1.0);
                continue;
            }

            ProposerGauge::GamesResolved.increment(1.0);
        }

        Ok(())
    }

    /// Attempt to claim proposer bonds for any games flagged for claiming
    async fn claim_bonds(&self) -> Result<()> {
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
                    l2_block_end = %game.l2_block,
                    ?error,
                    "Failed to claim bond for game"
                );
                ProposerGauge::BondClaimingError.increment(1.0);
                continue;
            }

            ProposerGauge::GamesBondsClaimed.increment(1.0);
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
            l2_block_end = %game.l2_block,
            tx_hash = ?receipt.transaction_hash,
            "Game resolved successfully"
        );

        Ok(())
    }

    /// Submit the on-chain transaction to claim the proposer's bond for a given game.
    #[tracing::instrument(name = "[[Claiming Proposer Bonds]]", skip(self, game))]
    pub async fn submit_bond_claim_transaction(&self, game: &Game) -> Result<()> {
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
            l2_block_end = %game.l2_block,
            tx_hash = ?receipt.transaction_hash,
            "Bond claimed successfully"
        );

        Ok(())
    }

    /// Fetch game from the factory.
    ///
    /// Drop game if the game type is invalid or the output root is not valid.
    /// Drop game if the parent game does not exist.
    async fn fetch_game(&self, index: U256) -> Result<()> {
        let game = self.factory.gameAtIndex(index).call().await?;
        let game_address = game.proxy;
        let contract = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

        let l2_block = contract.l2BlockNumber().call().await?;
        let output_root = self.l2_provider.compute_output_root_at_block(l2_block).await?;
        let claim = contract.rootClaim().call().await?;
        let (parent_index, proposal_status, deadline) = match contract.claimData().call().await {
            Ok(data) => (data.parentIndex, data.status, U256::from(data.deadline).to::<u64>()),
            Err(error) => {
                tracing::debug!(game_index = %index, ?game_address, ?error,
                    "Falling back to legacy game with dummy claim data");
                (u32::MAX, ProposalStatus::Unchallenged, 0)
            }
        };

        let was_respected = contract.wasRespectedGameTypeWhenCreated().call().await?;
        let status = contract.status().call().await?;

        let mut state = self.state.lock().await;

        if !was_respected || output_root != claim {
            tracing::debug!(game_index = %index, ?game_address,
                "Dropping game due to invalid game type or output root");
            state.cursor = Some(index);
            return Ok(());
        }

        if parent_index != u32::MAX {
            let parent_idx = U256::from(parent_index);
            if !state.games.contains_key(&parent_idx) {
                tracing::debug!(game_index = %index, ?game_address,
                    parent_index = %parent_idx, "Dropping game due to missing parent");
                state.cursor = Some(index);
                return Ok(());
            }
        }

        tracing::info!(game_index = %index, ?game_address, parent_index = %parent_index, l2_block = %l2_block, ?status, ?proposal_status, deadline = %deadline, "Adding game to cache");
        state.games.insert(
            index,
            Game {
                index,
                address: game_address,
                parent_index,
                l2_block,
                status,
                proposal_status,
                deadline,
                should_attempt_to_resolve: false,
                should_attempt_to_claim_bond: false,
            },
        );

        state.cursor = Some(index);

        Ok(())
    }

    /// Handles the creation of a new game if conditions are met.
    #[tracing::instrument(name = "[[Proposing]]", skip(self))]
    pub async fn handle_game_creation(
        &self,
        mut next_l2_block_number_for_proposal: U256,
        parent_game_index: u32,
    ) -> Result<()> {
        let mut output_root = self
            .l2_provider
            .compute_output_root_at_block(next_l2_block_number_for_proposal)
            .await?;
        let mut extra_data =
            (next_l2_block_number_for_proposal, parent_game_index).abi_encode_packed();
        let mut maybe_existing_game = self
            .factory
            .games(self.config.game_type, output_root, extra_data.clone().into())
            .call()
            .await?
            .proxy;

        // If there already exists a game at the next L2 block number for proposal, increment the L2
        // block number by 1
        while maybe_existing_game != Address::ZERO {
            next_l2_block_number_for_proposal += U256::from(1);
            output_root = self
                .l2_provider
                .compute_output_root_at_block(next_l2_block_number_for_proposal)
                .await?;
            extra_data = (next_l2_block_number_for_proposal, parent_game_index).abi_encode_packed();
            maybe_existing_game = self
                .factory
                .games(self.config.game_type, output_root, extra_data.clone().into())
                .call()
                .await?
                .proxy;
        }

        tracing::info!(
            l2_block_number = %next_l2_block_number_for_proposal,
            parent_game_index = %parent_game_index,
            output_root = ?output_root,
            "Creating game"
        );

        self.create_game(output_root, extra_data).await?;

        Ok(())
    }

    /// Fetch the proposer metrics.
    async fn fetch_proposer_metrics(&self) -> Result<()> {
        let (canonical_head_l2_block, anchor_game) = {
            let state = self.state.lock().await;
            (state.canonical_head_l2_block, state.anchor_game.clone())
        };

        if let Some(canonical_head_l2_block) = canonical_head_l2_block {
            ProposerGauge::LatestGameL2BlockNumber.set(canonical_head_l2_block.to::<u64>() as f64);

            if let Some(finalized_l2_block_number) = self
                .host
                .get_finalized_l2_block_number(&self.fetcher, canonical_head_l2_block.to::<u64>())
                .await?
            {
                ProposerGauge::FinalizedL2BlockNumber.set(finalized_l2_block_number as f64);
            }

            if let Some(anchor_game) = anchor_game {
                ProposerGauge::AnchorGameL2BlockNumber.set(anchor_game.l2_block.to::<u64>() as f64);
            } else {
                ProposerGauge::AnchorGameL2BlockNumber.set(0.0);
            }
        } else {
            tracing::warn!("canonical_head_l2_block is None; skipping metrics update");
        }

        // Update active proving tasks metric
        let active_proving = self.count_active_proving_tasks().await;
        ProposerGauge::ActiveProvingTasks.set(active_proving as f64);

        Ok(())
    }

    /// Count active proving tasks
    async fn count_active_proving_tasks(&self) -> u64 {
        let tasks = self.tasks.lock().await;
        tasks.iter().filter(|(_, (_, info))| matches!(info, TaskInfo::GameProving { .. })).count()
            as u64
    }

    /// Count active defense tasks
    async fn count_active_defense_tasks(&self) -> u64 {
        let tasks = self.tasks.lock().await;
        tasks
            .iter()
            .filter(|(_, (_, info))| matches!(info, TaskInfo::GameProving { is_defense: true, .. }))
            .count() as u64
    }

    /// Spawn a dedicated metrics collection task
    fn spawn_metrics_collector(&self) {
        let proposer_metrics = self.clone();
        tokio::spawn(async move {
            let mut metrics_timer = time::interval(Duration::from_secs(15));
            loop {
                metrics_timer.tick().await;
                if let Err(e) = proposer_metrics.fetch_proposer_metrics().await {
                    tracing::warn!("Failed to fetch metrics: {:?}", e);
                    ProposerGauge::MetricsError.increment(1.0);
                }
            }
        });
    }

    /// Handle completed tasks and clean them up
    async fn handle_completed_tasks(&self) -> Result<()> {
        let mut tasks = self.tasks.lock().await;
        let mut completed = Vec::new();

        // Find completed tasks
        for (id, (handle, _)) in tasks.iter() {
            if handle.is_finished() {
                completed.push(*id);
            }
        }

        // Process completed tasks
        for id in completed {
            if let Some((handle, info)) = tasks.remove(&id) {
                match handle.await {
                    Ok(Ok(())) => {
                        tracing::info!("Task {:?} completed successfully", info);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("Task {:?} failed: {:?}", info, e);
                        // Handle task failure based on type
                        self.handle_task_failure(&info, e).await?;
                    }
                    Err(panic) => {
                        tracing::error!("Task {:?} panicked: {:?}", info, panic);
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle task failure based on task type
    async fn handle_task_failure(&self, info: &TaskInfo, _error: anyhow::Error) -> Result<()> {
        match info {
            TaskInfo::GameCreation { .. } => {
                ProposerGauge::GameCreationError.increment(1.0);
            }
            TaskInfo::GameProving { .. } => {
                ProposerGauge::GameProvingError.increment(1.0);
            }
            TaskInfo::GameResolution => {
                ProposerGauge::GameResolutionError.increment(1.0);
            }
            TaskInfo::BondClaim => {
                ProposerGauge::BondClaimingError.increment(1.0);
            }
        }
        Ok(())
    }

    /// Spawn pending operations if not already running
    async fn spawn_pending_operations(&self) -> Result<()> {
        // Check if we should create a game and spawn task if needed
        if !self.has_active_task_of_type(&TaskInfo::GameCreation { block_number: U256::ZERO }).await
        {
            match self.spawn_game_creation_task().await {
                Ok(true) => tracing::info!("Successfully spawned game creation task"),
                Ok(false) => {
                    tracing::debug!("No game creation needed - proposal interval not elapsed")
                }
                Err(e) => tracing::warn!("Failed to spawn game creation task: {:?}", e),
            }
        } else {
            tracing::info!("Game creation task already active");
        }

        // Check if we should defend games
        match self.spawn_game_defense_tasks().await {
            Ok(true) => tracing::info!("Successfully spawned game defense tasks"),
            Ok(false) => tracing::debug!("No games need defense or task already active"),
            Err(e) => tracing::warn!("Failed to spawn game defense tasks: {:?}", e),
        }

        // Spawn game resolution task
        if !self.has_active_task_of_type(&TaskInfo::GameResolution).await {
            if let Err(e) = self.spawn_game_resolution_task().await {
                tracing::warn!("Failed to spawn game resolution task: {:?}", e);
            } else {
                tracing::info!("Successfully spawned game resolution task");
            }
        }

        // Spawn bond claim task
        if !self.has_active_task_of_type(&TaskInfo::BondClaim).await {
            if let Err(e) = self.spawn_bond_claim_task().await {
                tracing::warn!("Failed to spawn bond claim task: {:?}", e);
            } else {
                tracing::info!("Successfully spawned bond claim task");
            }
        } else {
            tracing::info!("Bond claim task already active");
        }

        Ok(())
    }

    /// Check if there's an active task of the given type
    async fn has_active_task_of_type(&self, task_type: &TaskInfo) -> bool {
        let tasks = self.tasks.lock().await;
        tasks
            .values()
            .any(|(_, info)| std::mem::discriminant(info) == std::mem::discriminant(task_type))
    }

    /// Log current task statistics
    async fn log_task_stats(&self) {
        let tasks = self.tasks.lock().await;
        let active_count = tasks.len();
        if active_count > 0 {
            let mut task_counts: HashMap<&str, usize> = HashMap::new();
            let mut proving_games: Vec<String> = Vec::new();

            for (_, (_, info)) in tasks.iter() {
                let task_type = match info {
                    TaskInfo::GameCreation { .. } => "GameCreation",
                    TaskInfo::GameProving { game_address, .. } => {
                        proving_games.push(format!("{game_address:?}"));
                        "GameProving"
                    }
                    TaskInfo::GameResolution => "GameResolution",
                    TaskInfo::BondClaim => "BondClaim",
                };
                *task_counts.entry(task_type).or_insert(0) += 1;
            }

            let task_types: Vec<String> = task_counts
                .into_iter()
                .map(|(type_name, count)| format!("{type_name}: {count}"))
                .collect();

            tracing::info!("Active tasks: {} ({})", active_count, task_types.join(", "));

            // Log specific games being proven
            if !proving_games.is_empty() {
                tracing::info!("Games being proven: {}", proving_games.join(", "));
            }
        }
    }

    /// Spawn a game creation task if conditions are met
    ///
    /// Returns:
    /// - Ok(true): Task was successfully spawned
    /// - Ok(false): No work needed (proposal interval not elapsed or no finalized blocks)
    /// - Err: Actual error occurred during task spawning
    async fn spawn_game_creation_task(&self) -> Result<bool> {
        // First check if we should create a game
        let (should_create, next_l2_block_number_for_proposal, parent_game_index) =
            self.should_create_game().await?;
        if !should_create {
            return Ok(false);
        }

        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            if let Err(e) = proposer
                .handle_game_creation(next_l2_block_number_for_proposal, parent_game_index)
                .await
            {
                tracing::warn!("Failed to handle game creation: {:?}", e);
                return Err(e);
            }

            ProposerGauge::GamesCreated.increment(1.0);
            Ok(())
        });

        let task_info = TaskInfo::GameCreation { block_number: next_l2_block_number_for_proposal };

        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!(
            "Spawned game creation task {} for block {}",
            task_id,
            next_l2_block_number_for_proposal
        );
        Ok(true)
    }

    /// Check if we should create a game
    ///
    /// In fast finality mode, prioritizes proving existing games over creating new ones, preventing
    /// the proposer from abandoning in-progress games after a restart.
    ///
    /// Then compares the next L2 block number for proposal with the finalized L2 block number.
    /// If the finalized L2 block number is greater than or equal to the next L2 block number for
    /// proposal, we should create a game.
    ///
    /// Returns boolean indicating if a game should be created, the next L2 block number for
    /// proposal, and the parent game index.
    /// If a game should not be created, dummy values are returned for the next L2 block number for
    /// proposal and parent game index.
    async fn should_create_game(&self) -> Result<(bool, U256, u32)> {
        // In fast finality mode, resume proving for existing games before creating new ones
        // TODO(fakedev9999): Consider unifying proving concurrency control for both fast finality
        // and defense proving with a priority system.
        if self.config.fast_finality_mode {
            let mut active_proving = self.count_active_proving_tasks().await;

            // Resume proving for existing unproven games before creating new ones.
            if active_proving < self.config.fast_finality_proving_limit {
                let signer_address = self.signer.address();

                let unproven_games = {
                    let state = self.state.lock().await;
                    let tasks = self.tasks.lock().await;

                    let candidates = state
                        .games
                        .values()
                        .filter(|game| game.status == GameStatus::IN_PROGRESS)
                        .filter(|game| game.proposal_status == ProposalStatus::Unchallenged)
                        .map(|game| (game.index, game.address))
                        .collect::<Vec<_>>();

                    let proving_set = tasks
                        .values()
                        .filter_map(|(_, info)| match info {
                            TaskInfo::GameProving { game_address, .. } => Some(*game_address),
                            _ => None,
                        })
                        .collect::<HashSet<_>>();

                    // Filter games being proven
                    candidates
                        .into_iter()
                        .filter(|(_, address)| !proving_set.contains(address))
                        .collect::<Vec<_>>()
                };

                let mut spawned_count = 0;

                for (index, game_address) in unproven_games {
                    if active_proving >= self.config.fast_finality_proving_limit {
                        tracing::debug!(
                            "Reached fast finality proving capacity ({}/{}) while resuming games",
                            active_proving,
                            self.config.fast_finality_proving_limit
                        );
                        break;
                    }

                    // Check if we own this game
                    let contract =
                        OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
                    let creator = match contract.gameCreator().call().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                ?game_address,
                                ?e,
                                "Failed to check game creator, skipping"
                            );
                            continue;
                        }
                    };

                    if creator != signer_address {
                        continue;
                    }

                    // Spawn proving task
                    match self.spawn_game_proving_task(game_address, false).await {
                        Ok(()) => {
                            tracing::info!(
                                game_address = ?game_address,
                                game_index = %index,
                                "Resumed fast finality proving for existing game"
                            );
                            spawned_count += 1;
                            active_proving += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                ?game_address,
                                ?e,
                                "Failed to spawn proving task, continuing"
                            );
                        }
                    }
                }

                if spawned_count > 0 {
                    tracing::info!(
                        "Resumed proving for {} existing game(s), now at {}/{} capacity",
                        spawned_count,
                        active_proving,
                        self.config.fast_finality_proving_limit
                    );
                }
            }

            // Check capacity after resuming existing games
            if active_proving >= self.config.fast_finality_proving_limit {
                tracing::info!(
                    "Skipping game creation: at proving capacity ({}/{})",
                    active_proving,
                    self.config.fast_finality_proving_limit
                );
                return Ok((false, U256::ZERO, u32::MAX));
            }
        }

        let (canonical_head_l2_block, parent_game_index) = {
            let state = self.state.lock().await;

            let Some(canonical_head_l2_block) = state.canonical_head_l2_block else {
                tracing::info!("No canonical head; skipping game creation");
                return Ok((false, U256::ZERO, u32::MAX));
            };

            let parent_game_index =
                state.canonical_head_index.map(|index| index.to::<u32>()).unwrap_or(u32::MAX);

            (canonical_head_l2_block, parent_game_index)
        };

        let next_l2_block_number_for_proposal =
            canonical_head_l2_block + U256::from(self.config.proposal_interval_in_blocks);

        let finalized_l2_head_block_number = self
            .host
            .get_finalized_l2_block_number(&self.fetcher, canonical_head_l2_block.to::<u64>())
            .await?;

        Ok((
            finalized_l2_head_block_number
                .map(|finalized_block| {
                    U256::from(finalized_block) >= next_l2_block_number_for_proposal
                })
                .unwrap_or(false),
            next_l2_block_number_for_proposal,
            parent_game_index,
        ))
    }

    /// Spawn game defense tasks if needed
    ///
    /// Returns:
    /// - Ok(true): Defense task was successfully spawned
    /// - Ok(false): No work needed (no defensible games or task already exists)
    /// - Err: Actual error occurred during task spawning
    #[tracing::instrument(name = "[[Defending]]", skip(self))]
    async fn spawn_game_defense_tasks(&self) -> Result<bool> {
        // Check if there are games needing defense
        let candidates = {
            let state = self.state.lock().await;
            state
                .games
                .values()
                .filter(|game| game.status == GameStatus::IN_PROGRESS)
                .filter(|game| matches!(game.proposal_status, ProposalStatus::Challenged))
                .map(|game| (game.index, game.address))
                .collect::<Vec<_>>()
        };

        let mut active_defense_tasks_count = self.count_active_defense_tasks().await;
        let max_concurrent = self.config.max_concurrent_defense_tasks;

        let mut tasks_spawned = false;

        for (index, game_address) in candidates {
            if active_defense_tasks_count >= max_concurrent {
                tracing::debug!(
                    "The max concurrent defense tasks count ({}) has been reached",
                    max_concurrent
                );
                break;
            }

            if self.has_active_proving_for_game(game_address).await {
                continue;
            }

            tracing::info!(
                game_address = ?game_address,
                game_index = %index,
                "Spawning defense for challenged game"
            );
            self.spawn_game_proving_task(game_address, true).await?;
            active_defense_tasks_count += 1;
            tasks_spawned = true;
        }

        Ok(tasks_spawned)
    }

    /// Check if there's an active proving task for a specific game
    async fn has_active_proving_for_game(&self, game_address: Address) -> bool {
        let tasks = self.tasks.lock().await;
        tasks.values().any(|(_, info)| {
            matches!(info, TaskInfo::GameProving { game_address: addr, .. } if *addr == game_address)
        })
    }

    /// Spawn a game proving task for a specific game
    async fn spawn_game_proving_task(&self, game_address: Address, is_defense: bool) -> Result<()> {
        let proposer: OPSuccinctProposer<P, H> = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        // Get the game block number to include in logs
        let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
        let l2_block_number = game.l2BlockNumber().call().await?;
        let start_block = l2_block_number.to::<u64>() - self.config.proposal_interval_in_blocks;
        let end_block = l2_block_number.to::<u64>();

        tracing::info!(
            "Spawning game proving task {} for game {:?} (blocks {}-{})",
            task_id,
            game_address,
            start_block,
            end_block
        );

        // In mock mode, use spawn_blocking for CPU-intensive mock proof generation
        // In network mode, use spawn for async network operations
        let handle = if proposer.config.mock_mode {
            tokio::task::spawn_blocking(move || {
                // Use a runtime for the blocking task to handle async operations
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async move {
                    let start_time = std::time::Instant::now();
                    let (tx_hash, total_instruction_cycles, total_sp1_gas) =
                        proposer.prove_game(game_address).await?;

                    // Record successful proving
                    ProposerGauge::GamesProven.increment(1.0);
                    ProposerGauge::ProvingDurationSeconds.set(start_time.elapsed().as_secs_f64());

                    tracing::info!(
                        game_address = ?game_address,
                        l2_block_start = start_block,
                        l2_block_end = end_block,
                        tx_hash = ?tx_hash,
                        duration_s = start_time.elapsed().as_secs_f64(),
                        total_instruction_cycles = total_instruction_cycles,
                        total_sp1_gas = total_sp1_gas,
                        "Game proven successfully"
                    );
                    Ok(())
                })
            })
        } else {
            tokio::spawn(async move {
                let start_time = std::time::Instant::now();
                let (tx_hash, total_instruction_cycles, total_sp1_gas) =
                    proposer.prove_game(game_address).await?;

                // Record successful proving
                ProposerGauge::GamesProven.increment(1.0);
                ProposerGauge::ProvingDurationSeconds.set(start_time.elapsed().as_secs_f64());

                tracing::info!(
                    game_address = ?game_address,
                    l2_block_start = start_block,
                    l2_block_end = end_block,
                    tx_hash = ?tx_hash,
                    duration_s = start_time.elapsed().as_secs_f64(),
                    total_instruction_cycles = total_instruction_cycles,
                    total_sp1_gas = total_sp1_gas,
                    "Game proven successfully"
                );
                Ok(())
            })
        };

        let task_info = TaskInfo::GameProving { game_address, is_defense };
        self.tasks.lock().await.insert(task_id, (handle, task_info));
        Ok(())
    }

    /// Spawn a game resolution task
    #[tracing::instrument(name = "[[Proposer Resolving]]", skip(self))]
    async fn spawn_game_resolution_task(&self) -> Result<()> {
        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move { proposer.resolve_games().await });

        let task_info = TaskInfo::GameResolution;
        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!("Spawned game resolution task {}", task_id);
        Ok(())
    }

    /// Spawn a bond claim task
    async fn spawn_bond_claim_task(&self) -> Result<()> {
        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move { proposer.claim_bonds().await });

        let task_info = TaskInfo::BondClaim;
        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!("Spawned bond claim task {}", task_id);
        Ok(())
    }
}
