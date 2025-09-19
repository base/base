use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::{Address, TxHash, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_sol_types::{SolEvent, SolValue};
use anyhow::{Context, Result};
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::{
    fetcher::OPSuccinctDataFetcher, get_agg_proof_stdin, host::OPSuccinctHost,
    metrics::MetricsGauge, witness_generation::WitnessGenerator,
};
use op_succinct_proof_utils::get_range_elf_embedded;
use op_succinct_signer_utils::Signer;
use sp1_sdk::{
    network::FulfillmentStrategy, NetworkProver, Prover, ProverClient, SP1ProofMode,
    SP1ProofWithPublicValues, SP1ProvingKey, SP1VerifyingKey, SP1_CIRCUIT_VERSION,
};
use tokio::{sync::Mutex, time};

use crate::{
    config::ProposerConfig,
    contract::{
        DisputeGameFactory::{DisputeGameCreated, DisputeGameFactoryInstance},
        OPSuccinctFaultDisputeGame,
    },
    prometheus::ProposerGauge,
    Action, FactoryTrait, L1Provider, L2Provider, L2ProviderTrait, Mode,
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
}

#[derive(Clone)]
pub struct OPSuccinctProposer<P, H: OPSuccinctHost>
where
    P: Provider + Clone + Send + Sync + 'static,
    H: OPSuccinctHost + Clone + Send + Sync + 'static,
{
    pub config: ProposerConfig,
    // The address being committed to when generating the aggregation proof to prevent
    // front-running attacks. This should be the same address that is being used to send
    // `prove` transactions.
    pub prover_address: Address,
    pub signer: Signer,
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
        network_private_key: String,
        prover_address: Address,
        signer: Signer,
        factory: DisputeGameFactoryInstance<P>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        host: Arc<H>,
    ) -> Result<Self> {
        let network_prover =
            Arc::new(ProverClient::builder().network().private_key(&network_private_key).build());
        let (range_pk, range_vk) = network_prover.setup(get_range_elf_embedded());
        let (agg_pk, _) = network_prover.setup(AGGREGATION_ELF);

        let l1_provider = ProviderBuilder::default().connect_http(config.l1_rpc.clone());
        let l2_provider = ProviderBuilder::default().connect_http(config.l2_rpc.clone());
        let init_bond = factory.fetch_init_bond(config.game_type).await?;

        Ok(Self {
            config: config.clone(),
            prover_address,
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
            },
            fetcher: fetcher.clone(),
            host,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_task_id: Arc::new(AtomicU64::new(1)),
        })
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
                .strategy(FulfillmentStrategy::Hosted)
                .skip_simulation(true)
                .cycle_limit(1_000_000_000_000)
                .gas_limit(1_000_000_000_000)
                .timeout(Duration::from_secs(4 * 60 * 60))
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
            self.prover_address,
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
                SP1ProofMode::Groth16,
                SP1_CIRCUIT_VERSION,
            )
        } else {
            self.prover
                .network_prover
                .prove(&self.prover.agg_pk, &sp1_stdin)
                .groth16()
                .timeout(Duration::from_secs(4 * 60 * 60))
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
    /// `l2_block_number`: the L2 block number we are proposing the output root for.
    /// `parent_game_index`: the index of the parent game.
    pub async fn create_game(
        &self,
        l2_block_number: U256,
        parent_game_index: u32,
    ) -> Result<Address> {
        tracing::info!(
            "Creating game at L2 block number: {:?}, with parent game index: {:?}",
            l2_block_number,
            parent_game_index
        );

        let extra_data = <(U256, u32)>::abi_encode_packed(&(l2_block_number, parent_game_index));

        let transaction_request = self
            .factory
            .create(
                self.config.game_type,
                self.l2_provider.compute_output_root_at_block(l2_block_number).await?,
                extra_data.into(),
            )
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
            l2_block_end = %l2_block_number,
            parent_index = parent_game_index,
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

    /// Handles the creation of a new game if conditions are met.
    /// Returns the address of the created game, if one was created.
    #[tracing::instrument(name = "[[Proposing]]", skip(self))]
    pub async fn handle_game_creation(&self) -> Result<Option<Address>> {
        // Get the latest valid proposal.
        let latest_valid_proposal = self
            .factory
            .get_latest_valid_proposal(self.l2_provider.clone(), self.config.game_type)
            .await?;

        // Determine next block number and parent game index.
        //
        // Two cases based on the result of `get_latest_valid_proposal`:
        // 1. With existing valid proposal:
        //    - Block number = latest valid proposal's block + proposal interval.
        //    - Parent = latest valid game's index.
        //
        // 2. Without valid proposal (first game or all existing games being faulty):
        //    - Block number = anchor L2 block number + proposal interval.
        //    - Parent = u32::MAX (special value indicating no parent).
        let (latest_proposed_block_number, next_l2_block_number_for_proposal, parent_game_index) =
            match latest_valid_proposal {
                Some((latest_block, latest_game_idx)) => (
                    latest_block,
                    latest_block + U256::from(self.config.proposal_interval_in_blocks),
                    latest_game_idx.to::<u32>(),
                ),
                None => {
                    let anchor_l2_block_number =
                        self.factory.get_anchor_l2_block_number(self.config.game_type).await?;
                    tracing::info!("Anchor L2 block number: {:?}", anchor_l2_block_number);
                    (
                        anchor_l2_block_number,
                        anchor_l2_block_number
                            .checked_add(U256::from(self.config.proposal_interval_in_blocks))
                            .unwrap(),
                        u32::MAX,
                    )
                }
            };

        let finalized_l2_head_block_number = self
            .host
            .get_finalized_l2_block_number(&self.fetcher, latest_proposed_block_number.to::<u64>())
            .await?;

        // There's always a new game to propose, as the chain is always moving forward from the
        // genesis block set for the game type. Only create a new game if the finalized L2
        // head block number is greater than the next L2 block number for proposal.
        if let Some(finalized_block) = finalized_l2_head_block_number {
            if U256::from(finalized_block) > next_l2_block_number_for_proposal {
                let game_address =
                    self.create_game(next_l2_block_number_for_proposal, parent_game_index).await?;

                Ok(Some(game_address))
            } else {
                tracing::info!("No new game to propose since proposal interval has not elapsed");

                Ok(None)
            }
        } else {
            tracing::info!("No new finalized block number found since last proposed block");
            Ok(None)
        }
    }

    /// Handles claiming bonds from resolved games.
    #[tracing::instrument(name = "[[Claiming Proposer Bonds]]", skip(self))]
    async fn handle_bond_claiming(&self) -> Result<Action> {
        if let Some(game_address) = self
            .factory
            .get_oldest_claimable_bond_game_address(
                self.config.game_type,
                self.config.max_games_to_check_for_bond_claiming,
                self.prover_address,
                Mode::Proposer,
                self.config.game_type,
            )
            .await?
        {
            tracing::info!(
                "Attempting to claim bond from game {:?} where proposer won",
                game_address
            );

            // Create a contract instance for the game
            let game = OPSuccinctFaultDisputeGame::new(game_address, self.l1_provider.clone());

            // Get L2 block number for context
            let l2_block_number = game.l2BlockNumber().call().await?;

            // Create a transaction to claim credit
            let transaction_request =
                game.claimCredit(self.prover_address).gas(200_000).into_transaction_request();

            // Sign and send the transaction
            match self
                .signer
                .send_transaction_request(self.config.l1_rpc.clone(), transaction_request)
                .await
            {
                Ok(receipt) => {
                    tracing::info!(
                        game_address = ?game_address,
                        l2_block_end = %l2_block_number,
                        tx_hash = ?receipt.transaction_hash,
                        "Bond claimed successfully"
                    );

                    Ok(Action::Performed)
                }
                Err(e) => {
                    tracing::error!(
                        game_address = ?game_address,
                        l2_block_end = %l2_block_number,
                        error = %e,
                        "Bond claiming failed"
                    );
                    Err(anyhow::anyhow!(
                        "Failed to claim proposer bond from game {:?}: {:?}",
                        game_address,
                        e
                    ))
                }
            }
        } else {
            tracing::info!("No games found where proposer won to claim bonds from");

            Ok(Action::Skipped)
        }
    }

    /// Fetch the proposer metrics.
    async fn fetch_proposer_metrics(&self) -> Result<()> {
        // Get the latest valid proposal.
        let latest_proposed_block_number = match self
            .factory
            .get_latest_valid_proposal(self.l2_provider.clone(), self.config.game_type)
            .await?
        {
            Some((l2_block_number, _game_index)) => l2_block_number,
            None => {
                tracing::info!("No valid proposals found for metrics");
                self.factory.get_anchor_l2_block_number(self.config.game_type).await?
            }
        };

        // Update metrics for latest game block number.
        ProposerGauge::LatestGameL2BlockNumber.set(latest_proposed_block_number.to::<u64>() as f64);

        // Update metrics for finalized L2 block number.
        if let Some(finalized_l2_block_number) = self
            .host
            .get_finalized_l2_block_number(&self.fetcher, latest_proposed_block_number.to::<u64>())
            .await?
        {
            ProposerGauge::FinalizedL2BlockNumber.set(finalized_l2_block_number as f64);
        }

        // Update metrics for anchor game block number.
        let anchor_game_l2_block_number =
            self.factory.get_anchor_l2_block_number(self.config.game_type).await?;
        ProposerGauge::AnchorGameL2BlockNumber.set(anchor_game_l2_block_number.to::<u64>() as f64);

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

    /// Runs the proposer indefinitely.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tracing::info!("OP Succinct Proposer running...");
        let mut interval = time::interval(Duration::from_secs(self.config.fetch_interval));

        // Spawn a dedicated task for continuous metrics collection
        self.spawn_metrics_collector();

        loop {
            interval.tick().await;

            // 1. Handle completed tasks
            if let Err(e) = self.handle_completed_tasks().await {
                tracing::warn!("Failed to handle completed tasks: {:?}", e);
            }

            // 2. Spawn new work (non-blocking)
            if let Err(e) = self.spawn_pending_operations().await {
                tracing::warn!("Failed to spawn pending operations: {:?}", e);
            }

            // 3. Log task statistics
            self.log_task_stats().await;
        }
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

        // Check if we should resolve games
        if !self.has_active_task_of_type(&TaskInfo::GameResolution).await {
            match self.spawn_game_resolution_task().await {
                Ok(true) => tracing::info!("Successfully spawned game resolution task"),
                Ok(false) => tracing::debug!("No games need resolution"),
                Err(e) => tracing::warn!("Failed to spawn game resolution task: {:?}", e),
            }
        } else {
            tracing::info!("Game resolution task already active");
        }

        // Check if we should claim bonds
        if !self.has_active_task_of_type(&TaskInfo::BondClaim).await {
            match self.spawn_bond_claim_task().await {
                Ok(true) => tracing::info!("Successfully spawned bond claim task"),
                Ok(false) => tracing::debug!("No bonds available to claim"),
                Err(e) => tracing::warn!("Failed to spawn bond claim task: {:?}", e),
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
        let should_create = self.should_create_game().await?;
        if !should_create {
            return Ok(false); // No work needed - normal case
        }

        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            match proposer.handle_game_creation().await {
                Ok(Some(_game_address)) => {
                    ProposerGauge::GamesCreated.increment(1.0);
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(e) => Err(anyhow::anyhow!("error in game creation: {:?}", e)),
            }
        });

        // Get the next proposal block for task info
        let next_block = self.get_next_proposal_block().await.unwrap_or(U256::ZERO);
        let task_info = TaskInfo::GameCreation { block_number: next_block };

        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!("Spawned game creation task {} for block {}", task_id, next_block);
        Ok(true)
    }

    /// Check if we should create a game
    async fn should_create_game(&self) -> Result<bool> {
        // In fast finality mode, check if we're at proving capacity
        // TODO(fakedev9999): Consider unifying proving concurrency control for both fast finality
        // and defense proving with a priority system.
        if self.config.fast_finality_mode {
            let active_proving = self.count_active_proving_tasks().await;
            if active_proving >= self.config.fast_finality_proving_limit {
                tracing::info!(
                    "Skipping game creation in fast finality mode: proving at capacity ({}/{})",
                    active_proving,
                    self.config.fast_finality_proving_limit
                );
                return Ok(false);
            }
        }

        // Use the existing logic from handle_game_creation
        let latest_valid_proposal = self
            .factory
            .get_latest_valid_proposal(self.l2_provider.clone(), self.config.game_type)
            .await?;

        let (latest_proposed_block_number, next_l2_block_number_for_proposal, _) =
            match latest_valid_proposal {
                Some((latest_block, latest_game_idx)) => (
                    latest_block,
                    latest_block + U256::from(self.config.proposal_interval_in_blocks),
                    latest_game_idx.to::<u32>(),
                ),
                None => {
                    let anchor_l2_block_number =
                        self.factory.get_anchor_l2_block_number(self.config.game_type).await?;
                    (
                        anchor_l2_block_number,
                        anchor_l2_block_number
                            .checked_add(U256::from(self.config.proposal_interval_in_blocks))
                            .unwrap(),
                        u32::MAX,
                    )
                }
            };

        let finalized_l2_head_block_number = self
            .host
            .get_finalized_l2_block_number(&self.fetcher, latest_proposed_block_number.to::<u64>())
            .await?;

        Ok(finalized_l2_head_block_number
            .map(|finalized_block| U256::from(finalized_block) > next_l2_block_number_for_proposal)
            .unwrap_or(false))
    }

    /// Get the next proposal block number
    async fn get_next_proposal_block(&self) -> Result<U256> {
        let latest_valid_proposal = self
            .factory
            .get_latest_valid_proposal(self.l2_provider.clone(), self.config.game_type)
            .await?;

        match latest_valid_proposal {
            Some((latest_block, _)) => {
                Ok(latest_block + U256::from(self.config.proposal_interval_in_blocks))
            }
            None => {
                let anchor_l2_block_number =
                    self.factory.get_anchor_l2_block_number(self.config.game_type).await?;
                Ok(anchor_l2_block_number
                    .checked_add(U256::from(self.config.proposal_interval_in_blocks))
                    .unwrap())
            }
        }
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
        let game_addresses = self
            .factory
            .get_defensible_game_addresses(
                self.config.max_games_to_check_for_defense,
                self.l1_provider.clone(),
                self.l2_provider.clone(),
                self.config.game_type,
            )
            .await?;

        let mut active_defense_tasks_count = self.count_active_defense_tasks().await;
        let mut tasks_spawned = false;
        for game_address in game_addresses {
            if active_defense_tasks_count >= self.config.max_concurrent_defense_tasks {
                tracing::debug!(
                    "The max concurrent proving tasks count ({}) has been reached",
                    self.config.max_concurrent_defense_tasks,
                );

                return Ok(tasks_spawned)
            }

            // Check if we already have a proving task for this game
            if !self.has_active_proving_for_game(game_address).await {
                self.spawn_game_proving_task(game_address, true).await?;
                active_defense_tasks_count += 1;
                tasks_spawned = true;
            }
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

    /// Spawn a game resolution task if needed
    ///
    /// Returns:
    /// - Ok(true): Resolution task was successfully spawned
    /// - Ok(false): No work needed (no games to resolve)
    /// - Err: Actual error occurred during task spawning
    #[tracing::instrument(name = "[[Proposer Resolving]]", skip(self))]
    async fn spawn_game_resolution_task(&self) -> Result<bool> {
        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            proposer
                .factory
                .resolve_games(
                    Mode::Proposer,
                    proposer.config.max_games_to_check_for_resolution,
                    proposer.signer.clone(),
                    proposer.config.l1_rpc.clone(),
                    proposer.l1_provider.clone(),
                    proposer.config.game_type,
                )
                .await
        });

        let task_info = TaskInfo::GameResolution;
        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!("Spawned game resolution task {}", task_id);
        Ok(true)
    }

    /// Spawn a bond claim task if needed
    ///
    /// Returns:
    /// - Ok(true): Bond claim task was successfully spawned
    /// - Ok(false): No work needed (no claimable bonds available)
    /// - Err: Actual error occurred during task spawning
    async fn spawn_bond_claim_task(&self) -> Result<bool> {
        // First check if there are bonds to claim
        let has_claimable_bonds = self
            .factory
            .get_oldest_claimable_bond_game_address(
                self.config.game_type,
                self.config.max_games_to_check_for_bond_claiming,
                self.prover_address,
                Mode::Proposer,
                self.config.game_type,
            )
            .await?
            .is_some();

        if !has_claimable_bonds {
            return Ok(false); // No bonds to claim - normal case
        }

        let proposer = self.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            match proposer.handle_bond_claiming().await {
                Ok(Action::Performed) => {
                    ProposerGauge::GamesBondsClaimed.increment(1.0);
                    Ok(())
                }
                Ok(Action::Skipped) => Ok(()),
                Err(e) => Err(e),
            }
        });

        let task_info = TaskInfo::BondClaim;
        self.tasks.lock().await.insert(task_id, (handle, task_info));
        tracing::info!("Spawned bond claim task {}", task_id);
        Ok(true)
    }
}
