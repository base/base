use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use tempfile::NamedTempFile;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, FixedBytes, TxHash, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_sol_types::{SolEvent, SolValue};
use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
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
use sp1_sdk::{Prover, ProverClient, SP1ProofWithPublicValues, SP1Stdin};
use tokio::{
    sync::{Mutex, RwLock, Semaphore},
    time,
};

use crate::{
    backup::ProposerBackup,
    config::ProposerConfig,
    contract::{
        AnchorStateRegistry::AnchorStateRegistryInstance,
        DisputeGameFactory::{DisputeGameCreated, DisputeGameFactoryInstance},
        GameStatus, OPSuccinctFaultDisputeGame, ProposalStatus,
    },
    is_parent_resolved,
    prometheus::ProposerGauge,
    prover::{MockProofProvider, NetworkProofProvider, ProofKeys, ProofProvider},
    FactoryTrait, L1Provider, L2Provider, L2ProviderTrait, TxErrorExt, TX_REVERTED_PREFIX,
};

/// Max allowed time (secs) between a game's deadline and the anchor game's deadline.
///
/// Games beyond this threshold are skipped during incremental syncs to cut startup latency and
/// avoid caching stale data.
///
/// The 14-day window is chosen with a 7-day challenge period in mind, plus a 7-day buffer,
/// ensuring all actionable games are included under normal conditions.
pub const MAX_GAME_DEADLINE_LAG: u64 = 60 * 60 * 24 * 14; // 14 days

/// Divisor for calculating deadline warning threshold.
///
/// When less than `max_duration / DEADLINE_WARNING_DIVISOR` time remains,
/// the deadline is considered "approaching".
pub const DEADLINE_WARNING_DIVISOR: u64 = 2;

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

/// Represents a dispute game in the on-chain game DAG.
///
/// Games form a directed acyclic graph where each game builds upon a parent game, extending the
/// chain with a new proposed output root. The proposer tracks these games to determine when to
/// propose new games, defend existing ones, resolve completed games and claim bonds.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
/// - `cursor`: Highest factory index processed in the prior sync. Each incremental sync walks
///   backward from the latest index to this value, then sets it to the new latest index.
/// - `games`: cached metadata for every tracked game keyed by index
#[derive(Default)]
struct ProposerState {
    anchor_game: Option<Game>,
    canonical_head_index: Option<U256>,
    canonical_head_l2_block: Option<U256>,
    cursor: Cursor,
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

/// Snapshot of the proposer's cached state for testing and monitoring.
#[derive(Clone, Debug)]
pub struct ProposerStateSnapshot {
    pub anchor_index: Option<U256>,
    pub canonical_head_index: Option<U256>,
    pub canonical_head_l2_block: U256,
    pub games: Vec<(U256, Address)>,
}

/// On-chain contract timing parameters.
#[derive(Clone, Debug)]
pub struct ContractParams {
    /// Maximum duration allowed for challenging a game.
    pub max_challenge_duration: u64,
    /// Maximum duration allowed for proving after a challenge.
    pub max_prove_duration: u64,
}

#[derive(Clone)]
pub struct OPSuccinctProposer<P, H: OPSuccinctHost>
where
    P: Provider + Clone + Send + Sync + 'static,
    H: OPSuccinctHost + Clone + Send + Sync + 'static,
{
    pub config: ProposerConfig,
    contract_params: OnceLock<ContractParams>,
    pub signer: SignerLock,
    pub l1_provider: L1Provider,
    pub l2_provider: L2Provider,
    pub anchor_state_registry: Arc<AnchorStateRegistryInstance<P>>,
    pub factory: Arc<DisputeGameFactoryInstance<P>>,
    init_bond: OnceLock<U256>,
    pub safe_db_fallback: bool,
    prover: ProofProvider,
    fetcher: Arc<OPSuccinctDataFetcher>,
    host: Arc<H>,
    tasks: Arc<Mutex<TaskMap>>,
    next_task_id: Arc<AtomicU64>,
    state: Arc<RwLock<ProposerState>>,
    backup_semaphore: Arc<Semaphore>,
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
        anchor_state_registry: AnchorStateRegistryInstance<P>,
        factory: DisputeGameFactoryInstance<P>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        host: Arc<H>,
    ) -> Result<Self> {
        // Set up the network prover.
        let network_signer = get_network_signer(config.use_kms_requester).await?;
        let network_mode = determine_network_mode(
            config.proof_provider.range_proof_strategy,
            config.proof_provider.agg_proof_strategy,
        )?;
        let network_prover = Arc::new(
            ProverClient::builder().network_for(network_mode).signer(network_signer).build(),
        );
        let (range_pk, range_vk) = network_prover.setup(get_range_elf_embedded());
        let (agg_pk, _) = network_prover.setup(AGGREGATION_ELF);

        let keys = ProofKeys {
            range_pk: Arc::new(range_pk),
            range_vk: Arc::new(range_vk),
            agg_pk: Arc::new(agg_pk),
        };

        let prover = if config.mock_mode {
            ProofProvider::Mock(MockProofProvider::new(
                network_prover,
                keys,
                config.proof_provider.clone(),
                AGGREGATION_ELF,
            ))
        } else {
            ProofProvider::Network(NetworkProofProvider::new(
                network_prover,
                keys,
                config.proof_provider.clone(),
                network_mode,
            ))
        };

        let l1_provider = ProviderBuilder::default().connect_http(config.l1_rpc.clone());
        let l2_provider = ProviderBuilder::default().connect_http(config.l2_rpc.clone());

        let initial_state = ProposerState::default();

        Ok(Self {
            config: config.clone(),
            contract_params: OnceLock::new(),
            signer,
            l1_provider,
            l2_provider,
            anchor_state_registry: Arc::new(anchor_state_registry),
            factory: Arc::new(factory.clone()),
            init_bond: OnceLock::new(),
            safe_db_fallback: config.safe_db_fallback,
            prover,
            fetcher: fetcher.clone(),
            host,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_task_id: Arc::new(AtomicU64::new(1)),
            state: Arc::new(RwLock::new(initial_state)),
            backup_semaphore: Arc::new(Semaphore::new(1)),
        })
    }

    /// Returns a lightweight snapshot of the proposer's cached state.
    pub async fn state_snapshot(&self) -> ProposerStateSnapshot {
        let state = self.state.read().await;
        ProposerStateSnapshot {
            anchor_index: state.anchor_game.as_ref().map(|game| game.index),
            canonical_head_index: state.canonical_head_index,
            canonical_head_l2_block: state.canonical_head_l2_block.unwrap_or(U256::ZERO),
            games: state.games.values().map(|game| (game.index, game.address)).collect(),
        }
    }

    /// Returns a copy of a game's full internal state for testing.
    #[cfg(feature = "integration")]
    pub async fn get_game(&self, index: U256) -> Option<Game> {
        let state = self.state.read().await;
        state.games.get(&index).cloned()
    }

    /// Spawns game defense tasks for testing. Returns true if any tasks were spawned.
    #[cfg(feature = "integration")]
    pub async fn spawn_defense_tasks_for_test(&self) -> Result<bool> {
        self.spawn_game_defense_tasks().await
    }

    /// Returns the list of game addresses with active defense proving tasks (for testing).
    #[cfg(feature = "integration")]
    pub async fn get_active_defense_game_addresses(&self) -> Vec<Address> {
        let tasks = self.tasks.lock().await;
        tasks
            .iter()
            .filter_map(|(_, (_, info))| match info {
                TaskInfo::GameProving { game_address, is_defense: true } => Some(*game_address),
                _ => None,
            })
            .collect()
    }

    /// Runs the proposer indefinitely.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tracing::info!("OP Succinct Proposer running...");

        self.try_init().await?;

        // Spawn a dedicated task for continuous metrics collection
        self.spawn_metrics_collector();

        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));
        loop {
            interval.tick().await;

            // 1. Synchronize cached dispute state before scheduling work.
            if let Err(e) = self.sync_state().await {
                tracing::warn!("Failed to sync proposer state: {:?}", e);
                continue
            }

            self.backup().await;

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

    /// Runs startup validations with retries before entering main loop.
    pub async fn try_init(&self) -> Result<()> {
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));
        let mut retry_count = 0u32;

        loop {
            match self.validate_and_init().await {
                Ok(()) => break,
                Err(e) => {
                    retry_count += 1;
                    if retry_count == 1 {
                        tracing::error!(attempt = retry_count, error = %e, "Startup validations failed");
                    } else {
                        tracing::warn!(
                            attempt = retry_count,
                            "Startup validations still pending, retrying..."
                        );
                    }
                    interval.tick().await;
                }
            }
        }

        Ok(())
    }

    /// Validates startup and initializes state.
    pub async fn validate_and_init(&self) -> Result<()> {
        let (anchor_l2_block, init_bond, contract_params) = self.startup_validations().await?;
        self.init_state(anchor_l2_block, init_bond, contract_params).await
    }

    /// Runs one-time startup validations before the proposer begins normal operations.
    /// Returns the validated anchor L2 block number, init bond, and contract params.
    async fn startup_validations(&self) -> Result<(U256, U256, ContractParams)> {
        // Validate anchor state registry matches factory's game implementation.
        Self::validate_anchor_state_registry(
            &self.anchor_state_registry,
            &self.factory,
            self.config.game_type,
        )
        .await?;

        // Fetch and validate anchor L2 block number.
        let anchor_l2_block = self.anchor_state_registry.getAnchorRoot().call().await?._1;
        Self::validate_anchor_l2_block(
            anchor_l2_block,
            &self.config,
            self.host.as_ref(),
            self.fetcher.as_ref(),
        )
        .await?;

        // Fetch init bond.
        let init_bond = self.factory.fetch_init_bond(self.config.game_type).await?;

        // Fetch contract params from game implementation.
        let game_impl = self.factory.game_impl(self.config.game_type).await?;
        let max_challenge_duration = game_impl.maxChallengeDuration().call().await?.to::<u64>();
        let max_prove_duration = game_impl.maxProveDuration().call().await?;
        let contract_params = ContractParams { max_challenge_duration, max_prove_duration };

        Ok((anchor_l2_block, init_bond, contract_params))
    }

    /// Initialize proposer state with the validated anchor L2 block, init bond, and contract
    /// params.
    async fn init_state(
        &self,
        anchor_l2_block: U256,
        init_bond: U256,
        contract_params: ContractParams,
    ) -> Result<()> {
        self.state.write().await.canonical_head_l2_block = Some(anchor_l2_block);
        self.init_bond
            .set(init_bond)
            .map_err(|_| anyhow::anyhow!("init_bond must not already be set"))?;
        self.contract_params
            .set(contract_params)
            .map_err(|_| anyhow::anyhow!("contract_params must not already be set"))?;

        // Validate backup path and restore state if available.
        if let Some(path) = &self.config.backup_path {
            // Validate parent directory exists.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    anyhow::bail!("backup path parent directory does not exist: {:?}", parent);
                }
            }

            // Validate path is writable by creating a temp file in the same directory.
            let dir = path.parent().unwrap_or(Path::new("."));
            NamedTempFile::new_in(dir)
                .with_context(|| format!("backup path is not writable: {:?}", path))?;

            // Restore state from backup if available.
            if let Some(restored) = ProposerState::try_restore(path) {
                let mut state = self.state.write().await;
                state.cursor = restored.cursor;
                state.games = restored.games;
                state.anchor_game = restored.anchor_game;
                ProposerGauge::BackupRestoreSuccess.increment(1.0);
            } else if path.exists() {
                // File exists but couldn't be parsed - this is an error.
                tracing::warn!(?path, "Failed to restore proposer state from backup");
                ProposerGauge::BackupRestoreError.increment(1.0);
            }
        }

        Ok(())
    }

    async fn validate_anchor_l2_block(
        anchor_l2_block: U256,
        config: &ProposerConfig,
        host: &H,
        fetcher: &OPSuccinctDataFetcher,
    ) -> Result<()> {
        let finalized_l2_block = host
            .get_finalized_l2_block_number(fetcher, anchor_l2_block.to::<u64>())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot fetch finalized L2 block number from L2 RPC: {}\n\
                     Please check that your L2 node is running and accessible.",
                    config.l2_rpc
                )
            })?;

        if anchor_l2_block > U256::from(finalized_l2_block) {
            return Err(anyhow::anyhow!(
                "Contract misconfiguration detected: Contract's anchor L2 block ({}) is ahead of \
                 the current finalized L2 block ({}). This indicates:\n\
                 1. The contract's startingL2BlockNumber is misconfigured to a future value, OR\n\
                 2. Your L2 node is not fully synced, OR\n\
                 3. Your L2 RPC endpoint is incorrect.\n\n\
                 Please verify your configuration before starting the proposer.",
                anchor_l2_block,
                finalized_l2_block
            ));
        }

        Ok(())
    }

    /// Validates that the provided anchor state registry matches the factory's game implementation.
    async fn validate_anchor_state_registry(
        anchor_state_registry: &AnchorStateRegistryInstance<P>,
        factory: &DisputeGameFactoryInstance<P>,
        game_type: u32,
    ) -> Result<()> {
        let game_impl = factory.game_impl(game_type).await?;
        let expected_registry = game_impl.anchorStateRegistry().call().await?;
        if *anchor_state_registry.address() != expected_registry {
            anyhow::bail!(
                "Anchor state registry address mismatch: config has {}, but factory's game implementation uses {}",
                anchor_state_registry.address(),
                expected_registry
            );
        }
        Ok(())
    }

    /// Synchronizes the proposer's cached view of the dispute-game tree with the on-chain state.
    ///
    /// Steps run in order:
    /// 1. `sync_games` pulls newly created games and refreshes cached metadata.
    /// 2. `sync_anchor_game` aligns the cached anchor pointer with the registry contract.
    /// 3. `compute_canonical_head` recomputes the head game used for proposal selection.
    pub async fn sync_state(&self) -> Result<()> {
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
    ///    - Incrementally fetch games from the factory, starting from the latest and working
    ///      backwards to the oldest unprocessed game, stopping at games exceeding the maximum
    ///      deadline lag from the anchor game (`MAX_GAME_DEADLINE_LAG`).
    ///    - Games are validated (correct type, valid output root) before being added.
    /// 2. Synchronize the status of all cached games.
    ///    - Games are removed (along with their subtree) if their parent is not in the cache.
    ///    - Games are marked for resolution if the parent is resolved, the game is over, and it's
    ///      own game.
    ///    - Games are marked for bond claim if they are finalized and there is credit to claim.
    /// 3. Evict games from the cache.
    ///    - Games that are finalized but there is no credit left to claim.
    ///    - The entire subtree of a CHALLENGER_WINS game.
    pub async fn sync_games(&self) -> Result<()> {
        // 1. Load new games.
        let latest_index = if let Some(index) = self.factory.fetch_latest_game_index().await? {
            Cursor::from(index)
        } else {
            return Ok(());
        };

        let anchor_address = self.anchor_state_registry.anchorGame().call().await?;

        let cursor = {
            let state = self.state.read().await;
            let current_cursor = state.cursor.clone();

            // This should never/rarely happen but in a case where the factory is redeployed/reset
            // while the proposer keeps running, the cursor is reset to zero to avoid skipping
            // any games.
            if latest_index < current_cursor {
                tracing::warn!(
                    latest_index = %latest_index,
                    current_cursor = %current_cursor,
                    "Factory reset suspected; resetting cursor to 0"
                );
                Cursor::none()
            } else {
                current_cursor
            }
        };

        let mut index = latest_index.clone();
        let mut anchor_deadline: Option<u64> = None;
        let mut invalid_game_ids = Vec::new();

        loop {
            if index == cursor {
                break;
            }

            let i = index.index().expect("must have an index here");
            let fetch_result = self.fetch_game(i).await?;

            match fetch_result {
                GameFetchResult::ValidGame { game_address, deadline } => {
                    // First time we hit the anchor, record its deadline
                    if game_address == anchor_address {
                        anchor_deadline = Some(deadline);
                    }

                    // Once we know the anchor deadline, enforce the lag constraint.
                    if let Some(anchor_d) = anchor_deadline {
                        if anchor_d.abs_diff(deadline) > MAX_GAME_DEADLINE_LAG {
                            tracing::debug!(
                                game_index = %index,
                                game_address = ?game_address,
                                game_deadline = %deadline,
                                anchor_deadline = %anchor_d,
                                "Game deadline exceeds max lag from anchor: stopping incremental fetch"
                            );
                            break;
                        }
                    }
                }
                GameFetchResult::UnsupportedType { game_address } => {
                    // Stop fetching once we find the anchor on an unsupported game.
                    if game_address == anchor_address {
                        break
                    }
                }
                GameFetchResult::InvalidGame { index } => {
                    invalid_game_ids.push(index);
                }
                GameFetchResult::AlreadyExists => {}
            }

            index.step_back();
        }

        {
            let mut state = self.state.write().await;
            state.cursor = latest_index;
        }

        if !invalid_game_ids.is_empty() {
            let mut state = self.state.write().await;
            for idx in invalid_game_ids {
                tracing::warn!(
                    game_index = %idx,
                    "Removing invalid game and its subtree from cache"
                );
                state.remove_subtree(idx);
            }
        }

        // 2. Synchronize the status of all cached games.
        let games = {
            let state = self.state.read().await;
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
                    self.anchor_state_registry.isGameFinalized(game_address).call().await?;

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
                            // Game removal policy:
                            // - Canonical head games are retained even with zero credit to maintain
                            //   chain consistency.
                            // - Anchor games are retained as they serve as the root of the dispute
                            //   game tree.
                            // - All other games with bonds already claimed are removed to free
                            //   cache memory.

                            let canonical_head_index = {
                                let state = self.state.read().await;
                                state.canonical_head_index
                            };

                            let should_remove = if canonical_head_index == Some(index) {
                                tracing::debug!(game_index = %index, "Retaining game: canonical head");
                                false
                            } else {
                                let anchor_game_address = self
                                    .anchor_state_registry
                                    .anchorGame()
                                    .call()
                                    .await
                                    .context("Failed to fetch anchor game for removal check")?;

                                if anchor_game_address == game_address {
                                    tracing::debug!(game_index = %index, "Retaining game: anchor game");
                                    false
                                } else {
                                    true
                                }
                            };

                            if should_remove {
                                actions.push(GameSyncAction::Remove(index));
                            } else {
                                actions.push(GameSyncAction::Update {
                                    index,
                                    status,
                                    proposal_status: claim_data.status,
                                    deadline,
                                    should_attempt_to_resolve: false,
                                    should_attempt_to_claim_bond: false,
                                });
                            }
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

            let mut state = self.state.write().await;
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
                        state.games.remove(&index);
                        tracing::debug!(game_index = %index, "Removed game from cache");
                    }
                    GameSyncAction::RemoveSubtree(index) => {
                        state.remove_subtree(index);
                    }
                }
            }
        }

        Ok(())
    }

    /// Synchronizes the anchor game from the registry.
    async fn sync_anchor_game(&self) -> Result<()> {
        let anchor_address = self.anchor_state_registry.anchorGame().call().await?;

        if anchor_address != Address::ZERO {
            let mut state = self.state.write().await;

            // Fetch the anchor game from the cache.
            if let Some((_, anchor_game)) =
                state.games.iter().find(|(_, game)| game.address == anchor_address)
            {
                state.anchor_game = Some(anchor_game.clone());
                tracing::debug!(?anchor_address, "Anchor game updated in cache");
            } else {
                tracing::debug!(?anchor_address, "Anchor game not in cache yet");
            }
        }

        Ok(())
    }

    /// Computes the canonical head by scanning all cached games.
    ///
    /// Canonical head is the game with the highest L2 block number. When an anchor game exists,
    /// the canonical head is chosen from its descendants, unless a non-descendant has a higher L2
    /// block number and an earlier lineage (parent is genesis or has a lower parent index than the
    /// best descendant).
    async fn compute_canonical_head(&self) {
        let mut state = self.state.write().await;

        let canonical_head = match state.anchor_game.as_ref() {
            None => state.games.values().max_by_key(|g| g.l2_block).cloned(),
            Some(anchor_game) => {
                let reachable = state.descendants_of(anchor_game.index);

                // Best among descendants
                let anchor_head = state
                    .games
                    .values()
                    .filter(|g| reachable.contains(&g.index))
                    .max_by_key(|g| g.l2_block);

                // Check non-descendants for override (higher block with genesis or lower parent)
                let override_head = anchor_head.and_then(|anchor| {
                    state
                        .games
                        .values()
                        .filter(|g| !reachable.contains(&g.index))
                        .filter(|g| {
                            g.l2_block > anchor.l2_block &&
                                (g.parent_index == u32::MAX ||
                                    g.parent_index < anchor.parent_index)
                        })
                        .max_by_key(|g| g.l2_block)
                });

                override_head.or(anchor_head).cloned()
            }
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
    pub async fn prove_game(
        &self,
        game_address: Address,
        start_block: u64,
        end_block: u64,
    ) -> Result<(TxHash, u64, u64)> {
        tracing::info!("Attempting to prove game {:?}", game_address);

        let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
        let l1_head_hash = game.l1Head().call().await?.0;
        tracing::debug!("L1 head hash: {:?}", hex::encode(l1_head_hash));

        let ranges = self
            .config
            .range_split_count
            .split(start_block, end_block)
            .context("failed to split range for proving")?;
        let num_ranges = ranges.len();
        tracing::info!("Proving over {num_ranges} ranges");

        let tasks = ranges.into_iter().enumerate().map(|(idx, (start, end))| {
            let this = self.clone();
            async move {
                tracing::info!("Generating Range Proof for blocks {start} to {end}");
                let sp1_stdin = this.range_proof_stdin(start, end, l1_head_hash.into()).await?;
                let (range_proof, inst_cycles, sp1_gas) =
                    this.prover.generate_range_proof(&sp1_stdin).await?;
                Ok::<_, anyhow::Error>((idx, range_proof, inst_cycles, sp1_gas))
            }
        });

        let max_concurrent = self.config.max_concurrent_range_proofs.get().min(num_ranges);
        let prove_stream = stream::iter(tasks);
        let results: Vec<(usize, SP1ProofWithPublicValues, u64, u64)> =
            prove_stream.buffer_unordered(max_concurrent).try_collect().await?;

        let mut proofs = vec![None; num_ranges];
        let mut boot_infos = vec![None; num_ranges];
        let mut total_instruction_cycles: u64 = 0;
        let mut total_sp1_gas: u64 = 0;

        for (idx, range_proof, inst_cycles, sp1_gas) in results {
            let proof = range_proof.proof.clone();
            let mut public_values = range_proof.public_values.clone();
            let boot_info: BootInfoStruct = public_values.read();

            proofs[idx] = Some(proof);
            boot_infos[idx] = Some(boot_info);
            total_instruction_cycles = total_instruction_cycles
                .checked_add(inst_cycles)
                .ok_or_else(|| anyhow::anyhow!("Instruction cycles overflow"))?;
            total_sp1_gas = total_sp1_gas
                .checked_add(sp1_gas)
                .ok_or_else(|| anyhow::anyhow!("SP1 gas overflow"))?;
        }

        let proofs = proofs
            .into_iter()
            .enumerate()
            .map(|(idx, proof)| {
                proof.ok_or_else(|| anyhow::anyhow!("missing proof for range index {idx}"))
            })
            .collect::<Result<Vec<_>>>()?;

        let boot_infos = boot_infos
            .into_iter()
            .enumerate()
            .map(|(idx, boot)| {
                boot.ok_or_else(|| anyhow::anyhow!("missing boot info for range index {idx}"))
            })
            .collect::<Result<Vec<_>>>()?;

        let latest_l1_head = boot_infos.last().context("No boot infos generated")?.l1Head;

        let headers = match self.fetcher.get_header_preimages(&boot_infos, latest_l1_head).await {
            Ok(headers) => headers,
            Err(e) => {
                tracing::error!("Failed to get header preimages: {e}");
                bail!("Failed to get header preimages: {e}");
            }
        };

        tracing::info!("Preparing Stdin for Agg Proof");
        let sp1_stdin = match get_agg_proof_stdin(
            proofs,
            boot_infos,
            headers,
            &self.prover.keys().range_vk,
            latest_l1_head,
            self.signer.address(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to get agg proof stdin: {e}");
                bail!("Failed to get agg proof stdin: {e}");
            }
        };

        let agg_proof = self.prover.generate_agg_proof(&sp1_stdin).await?;

        let transaction_request = game.prove(agg_proof.bytes().into()).into_transaction_request();
        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        if !receipt.status() {
            bail!("{TX_REVERTED_PREFIX} {receipt:?}");
        }

        Ok((receipt.transaction_hash, total_instruction_cycles, total_sp1_gas))
    }

    async fn range_proof_stdin(
        &self,
        start_block: u64,
        end_block: u64,
        l1_head_hash: B256,
    ) -> Result<SP1Stdin> {
        let host_args = self
            .host
            .fetch(start_block, end_block, Some(l1_head_hash), self.config.safe_db_fallback)
            .await
            .context("Failed to get host CLI args")?;

        let witness_data = match self.host.run(&host_args).await {
            Ok(witness) => witness,
            Err(e) => {
                tracing::error!("Failed to generate witness: {}", e);
                return Err(anyhow::anyhow!("Failed to generate witness: {}", e));
            }
        };

        let sp1_stdin = match self.host.witness_generator().get_sp1_stdin(witness_data) {
            Ok(stdin) => stdin,
            Err(e) => {
                tracing::error!("Failed to get proof stdin: {}", e);
                return Err(anyhow::anyhow!("Failed to get proof stdin: {}", e));
            }
        };

        Ok(sp1_stdin)
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
        let init_bond =
            *self.init_bond.get().context("init_bond must be set via startup_validations")?;
        let transaction_request = self
            .factory
            .create(self.config.game_type, output_root, extra_data.into())
            .value(init_bond)
            .into_transaction_request();

        let receipt = self
            .signer
            .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
            .await?;

        if !receipt.status() {
            bail!("{TX_REVERTED_PREFIX} {receipt:?}");
        }

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

            // Spawn a tracked proving task for the new game (None = just created, skip deadline
            // check)
            if let Err(e) = self.spawn_game_proving_task(game_address, false, None).await {
                tracing::warn!("Failed to spawn fast finality proof task: {:?}", e);
            }
        }

        Ok(game_address)
    }

    async fn resolve_games(&self) -> Result<()> {
        let candidates = {
            let state = self.state.read().await;
            state
                .games
                .values()
                .filter(|game| game.should_attempt_to_resolve)
                .cloned()
                .collect::<Vec<_>>()
        };

        for game in candidates {
            if let Err(error) = self.submit_resolution_transaction(&game).await {
                if error.is_revert() {
                    tracing::error!(
                        game_index = %game.index,
                        game_address = ?game.address,
                        l2_block_end = %game.l2_block,
                        ?error,
                        "Resolution tx included but reverted on-chain"
                    );
                } else {
                    tracing::warn!(
                        game_index = %game.index,
                        game_address = ?game.address,
                        l2_block_end = %game.l2_block,
                        ?error,
                        "Resolution tx unconfirmed (may be on-chain), will verify next cycle"
                    );
                }
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
            let state = self.state.read().await;
            state
                .games
                .values()
                .filter(|game| game.should_attempt_to_claim_bond)
                .cloned()
                .collect::<Vec<_>>()
        };

        for game in candidates {
            if let Err(error) = self.submit_bond_claim_transaction(&game).await {
                if error.is_revert() {
                    tracing::error!(
                        game_index = %game.index,
                        game_address = ?game.address,
                        l2_block_end = %game.l2_block,
                        ?error,
                        "Bond claim tx included but reverted on-chain"
                    );
                } else {
                    tracing::warn!(
                        game_index = %game.index,
                        game_address = ?game.address,
                        l2_block_end = %game.l2_block,
                        ?error,
                        "Bond claim tx unconfirmed (may be on-chain), will verify next cycle"
                    );
                }
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

        if !receipt.status() {
            bail!("{TX_REVERTED_PREFIX} {receipt:?}");
        }

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

        if !receipt.status() {
            bail!("{TX_REVERTED_PREFIX} {receipt:?}");
        }

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
    /// Drop game if:
    /// - The game type is not supported.
    /// - The game type does not respect the expected type when created.
    /// - The output root claim is invalid.
    pub async fn fetch_game(&self, index: U256) -> Result<GameFetchResult> {
        {
            let state = self.state.read().await;

            if state.games.contains_key(&index) {
                return Ok(GameFetchResult::AlreadyExists);
            }
        }

        let game = self.factory.gameAtIndex(index).call().await?;
        let game_address = game.proxy;
        let game_type = game.gameType;

        // Drop unsupported game types.
        if game_type != self.config.game_type {
            tracing::warn!(
                game_index = %index,
                ?game_address,
                game_type,
                expected_game_type = self.config.game_type,
                "Unsupported game type"
            );
            return Ok(GameFetchResult::UnsupportedType { game_address });
        }

        let contract = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

        let l2_block = contract.l2SequenceNumber().call().await?;
        let output_root = self.l2_provider.compute_output_root_at_block(l2_block).await?;
        let claim = contract.rootClaim().call().await?;
        let was_respected = contract.wasRespectedGameTypeWhenCreated().call().await?;
        let status = contract.status().call().await?;
        let claim_data = contract.claimData().call().await?;

        let (parent_index, proposal_status, deadline) = (
            claim_data.parentIndex,
            claim_data.status,
            U256::from(claim_data.deadline).to::<u64>(),
        );

        // Drop games whose type does not respect the expected type.
        if !was_respected {
            tracing::warn!(
                game_index = %index,
                ?game_address, game_type,
                expected_game_type = self.config.game_type,
                "Invalid game: game type was not respected when created"
            );
            return Ok(GameFetchResult::InvalidGame { index });
        }

        // Validate output root. If invalid, drop the game, setting the cursor to this index.
        if output_root != claim {
            tracing::warn!(
                game_index = %index,
                ?game_address,
                ?claim,
                expected_output_root = ?output_root,
                "Invalid game: root claim does not match computed output root"
            );
            return Ok(GameFetchResult::InvalidGame { index });
        }

        tracing::info!(
            game_index = %index,
            ?game_type,
            ?game_address,
            parent_index = %parent_index,
            l2_block = %l2_block,
            ?status,
            ?proposal_status,
            deadline = %deadline,
            "Valid game: adding to cache"
        );

        let mut state = self.state.write().await;
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

        Ok(GameFetchResult::ValidGame { game_address, deadline })
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
            let state = self.state.read().await;
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
                    let state = self.state.read().await;
                    let tasks = self.tasks.lock().await;

                    let candidates = state
                        .games
                        .values()
                        .filter(|game| game.status == GameStatus::IN_PROGRESS)
                        .filter(|game| game.proposal_status == ProposalStatus::Unchallenged)
                        .map(|game| (game.index, game.address, game.deadline))
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
                        .filter(|(_, address, _)| !proving_set.contains(address))
                        .collect::<Vec<_>>()
                };

                let mut spawned_count = 0;

                for (index, game_address, deadline) in unproven_games {
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
                    match self.spawn_game_proving_task(game_address, false, Some(deadline)).await {
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

        // Check if our game type matches the current respected game type.
        // The proposer should only create games when its type is the respected type.
        let respected_game_type = self.anchor_state_registry.respectedGameType().call().await?;
        if self.config.game_type != respected_game_type {
            tracing::warn!(
                proposer_game_type = self.config.game_type,
                ?respected_game_type,
                "Skipping game creation, game type does not match respected type"
            );
            return Ok((false, U256::ZERO, u32::MAX));
        }

        let (canonical_head_l2_block, parent_game_index) = {
            let state = self.state.read().await;

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

    /// Backup proposer state to disk in background. Skips if backup already in progress.
    async fn backup(&self) {
        let Some(path) = &self.config.backup_path else { return };

        let Ok(permit) = self.backup_semaphore.clone().try_acquire_owned() else {
            tracing::debug!("Skipping backup: previous backup still in progress");
            return;
        };

        let backup = self.state.read().await.to_backup();
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = backup.save(&path) {
                tracing::warn!("Failed to backup proposer state: {:?}", e);
                ProposerGauge::BackupSaveError.increment(1.0);
            } else {
                ProposerGauge::BackupSaveSuccess.increment(1.0);
            }
            drop(permit);
        });
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
        let mut candidates = {
            let state = self.state.read().await;
            state
                .games
                .values()
                .filter(|game| game.status == GameStatus::IN_PROGRESS)
                .filter(|game| matches!(game.proposal_status, ProposalStatus::Challenged))
                .map(|game| (game.index, game.address, game.deadline))
                .collect::<Vec<_>>()
        };
        // Sort by deadline ascending to prioritize games closest to expiring
        candidates.sort_unstable_by_key(|(_, _, deadline)| *deadline);

        let mut active_defense_tasks_count = self.count_active_defense_tasks().await;
        let max_concurrent = self.config.max_concurrent_defense_tasks;

        let mut tasks_spawned = false;

        for (index, game_address, deadline) in candidates {
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
            self.spawn_game_proving_task(game_address, true, Some(deadline)).await?;
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

    /// Spawn a game proving task for a specific game.
    ///
    /// Skips if game deadline has passed (too late to prove).
    async fn spawn_game_proving_task(
        &self,
        game_address: Address,
        is_defense: bool,
        deadline: Option<u64>,
    ) -> Result<()> {
        if let Some(deadline) = deadline {
            if self.should_skip_proving(game_address, deadline, is_defense)? {
                return Ok(());
            }
        }

        let proposer: OPSuccinctProposer<P, H> = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        // Get the game block number to include in logs
        let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());
        let starting_l2_block_number = game.startingBlockNumber().call().await?;
        let l2_block_number = game.l2SequenceNumber().call().await?;
        let start_block = starting_l2_block_number.to::<u64>();
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
                        proposer.prove_game(game_address, start_block, end_block).await?;

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
                    proposer.prove_game(game_address, start_block, end_block).await?;

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

    /// Check if proving should be skipped due to insufficient time remaining.
    ///
    /// Returns `Ok(true)` if proving should be skipped (deadline passed).
    /// Returns `Ok(false)` if proving should proceed.
    /// Logs a warning if the deadline is approaching.
    fn should_skip_proving(
        &self,
        game_address: Address,
        deadline: u64,
        is_defense: bool,
    ) -> Result<bool> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();

        let contract_params =
            self.contract_params.get().context("contract_params must be set via try_init")?;
        let max_duration = if is_defense {
            contract_params.max_prove_duration
        } else {
            contract_params.max_challenge_duration
        };

        let status = check_deadline_status(now, deadline, max_duration);

        match status {
            DeadlineStatus::Passed => {
                tracing::error!(
                    game_address = ?game_address,
                    deadline = deadline,
                    now = now,
                    "Game deadline passed, cannot prove"
                );
                Ok(true)
            }
            DeadlineStatus::Approaching { hours_remaining } => {
                tracing::warn!(
                    game_address = ?game_address,
                    is_defense = is_defense,
                    "Game deadline approaching, {:.1} hours remaining",
                    hours_remaining
                );
                ProposerGauge::DeadlineApproaching.increment(1.0);
                Ok(false)
            }
            DeadlineStatus::Ok => Ok(false),
        }
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

/// Result of fetching a game from the factory.
///
/// Games can either be added to the cache or dropped based on validation criteria.
pub enum GameFetchResult {
    /// Game was successfully validated and added to cache
    ValidGame { game_address: Address, deadline: u64 },
    /// Game type is unsupported
    UnsupportedType { game_address: Address },
    /// Game is invalid
    InvalidGame { index: U256 },
    /// Game was already present in the cache
    AlreadyExists,
}

/// Cursor that tracks dispute-game indices, representing the current position in the ordered
/// factory sequence.
///
/// Wraps `Option<U256>`:
/// - `Some(i)`: concrete position within the factory's ordered game sequence.
/// - `None`: sentinel meaning "no position" (before first game / past zero / uninitialized).
#[derive(Default, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    index: Option<U256>,
}

impl Cursor {
    /// Create a cursor with no index.
    pub fn none() -> Self {
        Cursor { index: None }
    }

    /// Get the current index of the cursor.
    pub fn index(&self) -> Option<U256> {
        self.index
    }

    /// Step the cursor back by one. If the cursor is at zero, it becomes `None`.
    pub fn step_back(&mut self) {
        if let Some(idx) = self.index {
            if idx > U256::ZERO {
                self.index = Some(idx.saturating_sub(U256::ONE));
            } else {
                self.index = None;
            }
        }
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.index {
            Some(idx) => write!(f, "{idx}"),
            None => write!(f, "None"),
        }
    }
}

impl From<U256> for Cursor {
    fn from(idx: U256) -> Self {
        Self { index: Some(idx) }
    }
}

/// Result of checking a game's deadline status.
#[derive(Debug, Clone, PartialEq)]
pub enum DeadlineStatus {
    /// Deadline has passed
    Passed,
    /// Deadline is approaching
    Approaching { hours_remaining: f64 },
    /// Deadline is not imminent
    Ok,
}

/// Check the deadline status for a game.
pub fn check_deadline_status(now: u64, deadline: u64, max_duration: u64) -> DeadlineStatus {
    if now >= deadline {
        return DeadlineStatus::Passed;
    }

    let time_remaining = deadline.saturating_sub(now);
    let warning_threshold = max_duration / DEADLINE_WARNING_DIVISOR;

    if time_remaining < warning_threshold {
        let hours_remaining = time_remaining as f64 / 3600.0;
        DeadlineStatus::Approaching { hours_remaining }
    } else {
        DeadlineStatus::Ok
    }
}

impl ProposerState {
    /// Serialize the current state to a backup struct.
    pub fn to_backup(&self) -> ProposerBackup {
        ProposerBackup::new(
            self.cursor.index(),
            self.games.values().cloned().collect(),
            self.anchor_game.as_ref().map(|g| g.index),
        )
    }

    /// Restore state from a backup struct.
    fn from_backup(backup: ProposerBackup) -> Self {
        let games: HashMap<U256, Game> = backup.games.into_iter().map(|g| (g.index, g)).collect();

        let anchor_game = backup.anchor_game_index.and_then(|idx| games.get(&idx).cloned());

        Self {
            cursor: backup.cursor.map(Cursor::from).unwrap_or_default(),
            games,
            anchor_game,
            // NOTE(fakedev9999): Not persisted; re-computed on first sync cycle from on-chain
            // state.
            canonical_head_index: None,
            canonical_head_l2_block: None,
        }
    }

    /// Try to restore state from a backup file. Returns None if file doesn't exist or is invalid.
    pub fn try_restore(path: &Path) -> Option<Self> {
        let backup = ProposerBackup::load(path)?;
        let state = Self::from_backup(backup);
        tracing::info!(
            ?path,
            games = state.games.len(),
            cursor = %state.cursor,
            "Proposer state restored from backup"
        );
        Some(state)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::RangeSplitCount;
    use anyhow::{bail, Result};
    use futures::stream::{self, StreamExt, TryStreamExt};
    use rstest::rstest;
    use std::time::Duration;

    async fn mock_prove(
        idx: usize,
        range: (u64, u64),
        fail: bool,
        delay: Duration,
    ) -> Result<usize> {
        tokio::time::sleep(delay).await;
        if fail {
            bail!("proof failed for range {}-{}", range.0, range.1);
        }
        Ok(idx)
    }

    async fn prove_ranges(
        ranges: Vec<(u64, u64)>,
        fail_idx: Option<usize>,
        concurrency: usize,
        delay: Duration,
    ) -> Result<Vec<usize>> {
        let tasks = ranges.into_iter().enumerate().map(|(idx, range)| {
            let fail = fail_idx == Some(idx);
            async move { mock_prove(idx, range, fail, delay).await }
        });
        stream::iter(tasks).buffer_unordered(concurrency).try_collect().await
    }

    #[rstest]
    #[case::first(0, "0-25")]
    #[case::middle(1, "25-50")]
    #[case::last(3, "75-100")]
    #[tokio::test]
    async fn test_failure_aborts(#[case] fail_idx: usize, #[case] expected: &str) {
        let ranges = RangeSplitCount::new(4).unwrap().split(0, 100).unwrap();
        let err = prove_ranges(ranges, Some(fail_idx), 4, Duration::ZERO).await.unwrap_err();
        assert!(err.to_string().contains(expected), "got: {err}");
    }

    #[rstest]
    #[case::full(16)]
    #[case::half(8)]
    #[case::single(1)]
    #[tokio::test]
    async fn test_stress_varying_concurrency(#[case] concurrency: usize) {
        let ranges = RangeSplitCount::new(16).unwrap().split(0, 1600).unwrap();
        let results =
            prove_ranges(ranges, None, concurrency, Duration::from_millis(5)).await.unwrap();

        let mut indices: Vec<_> = results.clone();
        indices.sort();
        assert_eq!(indices, (0..16).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn test_stress_repeated() {
        for _ in 0..50 {
            let ranges = RangeSplitCount::new(8).unwrap().split(0, 800).unwrap();
            let results = prove_ranges(ranges, None, 4, Duration::from_micros(100)).await.unwrap();
            assert_eq!(results.len(), 8);
        }
    }

    mod proving_deadline_tests {
        use super::super::{check_deadline_status, DeadlineStatus};
        use rstest::rstest;

        const HOUR: u64 = 3600;
        const MAX_DURATION: u64 = 6 * HOUR;

        #[rstest]
        #[case::passed_in_past(1000, 900, DeadlineStatus::Passed)]
        #[case::passed_exactly_now(1000, 1000, DeadlineStatus::Passed)]
        #[case::ok_plenty_of_time(1000, 1000 + 5 * HOUR, DeadlineStatus::Ok)]
        #[case::ok_at_threshold(1000, 1000 + MAX_DURATION / 2, DeadlineStatus::Ok)]
        fn test_deadline_status(
            #[case] now: u64,
            #[case] deadline: u64,
            #[case] expected: DeadlineStatus,
        ) {
            let status = check_deadline_status(now, deadline, MAX_DURATION);
            assert_eq!(status, expected);
        }

        #[rstest]
        #[case::two_hours_left(1000, 1000 + 2 * HOUR, 2.0)]
        #[case::one_hour_left(1000, 1000 + HOUR, 1.0)]
        fn test_deadline_approaching(
            #[case] now: u64,
            #[case] deadline: u64,
            #[case] expected_hours: f64,
        ) {
            match check_deadline_status(now, deadline, MAX_DURATION) {
                DeadlineStatus::Approaching { hours_remaining } => {
                    assert!((hours_remaining - expected_hours).abs() < 0.01);
                }
                other => panic!("Expected Approaching, got {:?}", other),
            }
        }
    }
}
