//! Implementation of the `basectl proofs` command group.

use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Write},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use alloy_primitives::{Address, B256};
use anyhow::Result;
use base_prover_service_protocol::{
    ExecutionStats, GetProofResponse, ListProofsRequest, ProofResult, ProofStatus, ProofSummary,
    ProofType, TeeKind, ZkBackend, ZkVm,
};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Serialize, Serializer};
use tracing::info;
use url::Url;

use crate::{
    CommandOutcome, Confirm, EXPECTED_RESOLUTION_NEVER, Format, GameDetails, GameListFilter,
    GameStatus, GameSummary, GamesClient, JsonOutput, KeyValueTable, MonitoringConfig,
    ProofProposeRequest, ProofsClient, ProofsCommandError, ProposalProofSubmitter,
    SnarkPlonkProofBytes, SubmitterKey,
};

/// How long `--wait` and `finalize` poll the prover service before giving up.
///
/// Network-backend PLONK proposal proofs regularly take hours (a compressed
/// range proof plus an aggregation/wrap stage), so the default client wait of
/// 30 minutes would time out on legitimate in-flight proofs.
const PROOF_MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

/// Request and inspect ZK proofs on the internal prover service.
#[derive(Debug, Args)]
pub struct ProofsCommand {
    /// Proof operation to run.
    #[command(subcommand)]
    pub command: ProofsCommands,
}

/// Prover-service proof request and inspection commands.
#[derive(Debug, Subcommand)]
pub enum ProofsCommands {
    /// Prove and finalize a dispute game in one shot from its address or creation transaction.
    Finalize(ProofsFinalizeArgs),
    /// Show status and result data for a submitted proof request.
    Status(ProofsStatusArgs),
    /// List submitted proof requests.
    List(ProofsListArgs),
    /// List recent dispute games on L1, or inspect one game.
    Games(ProofsGamesArgs),
    /// Request a game-matched PLONK proposal proof for an L1 dispute game.
    Propose(ProofsProposeArgs),
    /// Submit a completed PLONK proposal proof to its L1 dispute game.
    Submit(ProofsSubmitArgs),
}

/// Flags for `basectl proofs status`.
#[derive(Debug, Args)]
pub struct ProofsStatusArgs {
    /// Proof session ID returned by `basectl proofs finalize`.
    #[arg(value_name = "SESSION_ID")]
    pub session_id: String,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub prover_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub json: bool,
    /// With `--json`, emit the prover-service wire shape instead of the humanized summary.
    #[arg(long, requires = "json")]
    pub raw: bool,
}

/// Flags for `basectl proofs list`.
#[derive(Debug, Args)]
pub struct ProofsListArgs {
    /// Only list proofs with this status.
    #[arg(long, value_enum, value_name = "STATUS")]
    pub status: Option<ProofStatusFilter>,
    /// Number of rows to skip.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub offset: u64,
    /// Maximum rows to return.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: u32,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub prover_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl proofs games`.
#[derive(Debug, Args)]
pub struct ProofsGamesArgs {
    /// Dispute game proxy address to inspect. When omitted, lists recent games.
    #[arg(value_name = "GAME_ADDRESS")]
    pub game: Option<Address>,
    /// Maximum games to list, scanning backwards from the newest.
    #[arg(long, value_name = "N", default_value_t = 20, conflicts_with = "game", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=100))]
    pub limit: usize,
    /// Only list games of this game type.
    #[arg(long = "game-type", value_name = "TYPE", conflicts_with = "game")]
    pub game_type: Option<u32>,
    /// Only list games whose ZK proof slot is still empty.
    #[arg(long = "missing-zk", conflicts_with = "game")]
    pub missing_zk: bool,
    /// `DisputeGameFactory` address (overrides config `proofs.dispute_game_factory`).
    #[arg(long = "factory", value_name = "ADDRESS")]
    pub factory: Option<Address>,
    /// L1 RPC URL (overrides config `l1_rpc`).
    #[arg(long = "l1-rpc", value_name = "URL")]
    pub l1_rpc: Option<Url>,
    /// Emit humanized JSON instead of pretty text.
    #[arg(long)]
    pub json: bool,
}

/// Flags for `basectl proofs propose`.
#[derive(Debug, Args)]
pub struct ProofsProposeArgs {
    /// Dispute game proxy address to prove.
    #[arg(value_name = "GAME_ADDRESS")]
    pub game: Address,
    /// L1 wallet address that will submit the proof on chain.
    ///
    /// The proof journal commits to this address as the proposer, so the
    /// `verifyProposalProof` transaction must later be sent from exactly
    /// this wallet.
    #[arg(long = "prover-address", value_name = "ADDRESS")]
    pub prover_address: Address,
    /// ZK proving backend that executes the proof.
    ///
    /// Defaults to `network` (Succinct Prover Network, paid in PROVE)
    /// because proposal proofs are the standalone proving workflow;
    /// `cluster` uses a self-hosted SP1 cluster and `dry-run` executes
    /// locally without producing proof bytes.
    #[arg(
        long = "zk-backend",
        value_enum,
        value_name = "BACKEND",
        default_value_t = ZkBackendOption::Network
    )]
    pub zk_backend: ZkBackendOption,
    /// Explicit proof session ID (prover-service idempotency key).
    ///
    /// If omitted, basectl derives a deterministic session ID from the
    /// network name, ZK backend, game address, block range, checkpoint
    /// stride, and prover address, so re-running the same command resolves
    /// to the existing prover-service session instead of enqueueing a
    /// duplicate proof.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
    /// Allow an existing failed proof session to be requeued.
    ///
    /// Failed sessions are otherwise left unchanged so rerunning this command
    /// cannot accidentally purchase another proof.
    #[arg(long)]
    pub retry_failed: bool,
    /// Intermediate output root interval (checkpoint stride).
    ///
    /// Only needed when the game type has no registered implementation to
    /// read `INTERMEDIATE_BLOCK_INTERVAL` from; when it does, the flag must
    /// match that canonical value because a proof with any other stride
    /// would not verify on chain.
    #[arg(long = "intermediate-root-interval", value_name = "N")]
    pub intermediate_root_interval: Option<u64>,
    /// Poll the prover service until the proof succeeds or fails.
    ///
    /// Exits non-zero when the proof fails or does not complete in time.
    #[arg(long)]
    pub wait: bool,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub prover_rpc: Option<Url>,
    /// `DisputeGameFactory` address (overrides config `proofs.dispute_game_factory`).
    #[arg(long = "factory", value_name = "ADDRESS")]
    pub factory: Option<Address>,
    /// L1 RPC URL (overrides config `l1_rpc`).
    #[arg(long = "l1-rpc", value_name = "URL")]
    pub l1_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Flags for `basectl proofs submit`.
#[derive(Debug, Args)]
pub struct ProofsSubmitArgs {
    /// Dispute game proxy address to submit the proof to.
    #[arg(value_name = "GAME_ADDRESS")]
    pub game: Address,
    /// Path to a file holding the hex private key of the L1 wallet that
    /// signs and pays for the `verifyProposalProof` transaction.
    ///
    /// When omitted, the key is read from `BASECTL_SUBMITTER_PRIVATE_KEY`.
    /// The key is never accepted as a command-line value, so it cannot leak
    /// through shell history or the process list.
    ///
    /// The proof journal commits to the `--prover-address` passed to
    /// `basectl proofs propose`, so this key must control exactly that
    /// address or the contract rejects the proof with `InvalidSigner`.
    #[arg(long = "private-key-file", value_name = "PATH")]
    pub private_key_file: Option<PathBuf>,
    /// Explicit proof session ID to fetch from the prover service.
    ///
    /// If omitted, basectl derives the same deterministic session ID that
    /// `basectl proofs propose` derives from the network name, ZK backend,
    /// game address, block range, checkpoint stride, and submitter wallet
    /// address.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
    /// ZK backend the proof was proposed with (session ID derivation only).
    #[arg(
        long = "zk-backend",
        value_enum,
        value_name = "BACKEND",
        default_value_t = ZkBackendOption::Network
    )]
    pub zk_backend: ZkBackendOption,
    /// Intermediate output root interval the proof was proposed with
    /// (session ID derivation only).
    ///
    /// If omitted, the interval is read from the game type's registered
    /// implementation, matching `basectl proofs propose`.
    #[arg(long = "intermediate-root-interval", value_name = "N")]
    pub intermediate_root_interval: Option<u64>,
    /// Poll the prover service until the proof completes before submitting.
    #[arg(long)]
    pub wait: bool,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub prover_rpc: Option<Url>,
    /// `DisputeGameFactory` address (overrides config `proofs.dispute_game_factory`).
    #[arg(long = "factory", value_name = "ADDRESS")]
    pub factory: Option<Address>,
    /// L1 RPC URL (overrides config `l1_rpc`).
    #[arg(long = "l1-rpc", value_name = "URL")]
    pub l1_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Flags for `basectl proofs finalize`.
#[derive(Debug, Args)]
pub struct ProofsFinalizeArgs {
    /// Dispute game proxy address or its L1 creation transaction hash.
    ///
    /// A transaction target must be a mined, successful `createWithInitData`
    /// call sent directly to the `DisputeGameFactory`.
    #[arg(value_name = "GAME_OR_TX")]
    pub target: FinalizeTarget,
    /// Path to a file holding the hex private key of the L1 wallet that
    /// signs and pays for the `verifyProposalProof` transaction.
    ///
    /// When omitted, the key is read from `BASECTL_SUBMITTER_PRIVATE_KEY`.
    /// The key is never accepted as a command-line value, so it cannot leak
    /// through shell history or the process list.
    ///
    /// The proof journal commits to this wallet's address as the proposer,
    /// so the same key requests the proof and submits it on chain.
    #[arg(long = "private-key-file", value_name = "PATH")]
    pub private_key_file: Option<PathBuf>,
    /// ZK proving backend that executes the proof.
    ///
    /// Defaults to `network` (Succinct Prover Network, paid in PROVE)
    /// because proposal proofs are the standalone proving workflow;
    /// `cluster` uses a self-hosted SP1 cluster. `dry-run` is rejected
    /// because it produces no submittable proof bytes; use `proofs propose
    /// --zk-backend dry-run` for sizing.
    #[arg(
        long = "zk-backend",
        value_enum,
        value_name = "BACKEND",
        default_value_t = ZkBackendOption::Network
    )]
    pub zk_backend: ZkBackendOption,
    /// Explicit proof session ID (prover-service idempotency key).
    ///
    /// If omitted, basectl derives a deterministic session ID from the
    /// network name, ZK backend, game address, block range, checkpoint
    /// stride, and wallet address, so re-running the same command resumes
    /// the existing prover-service session instead of enqueueing a
    /// duplicate proof.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
    /// Retry a failed proof session with a new proof request.
    ///
    /// Without this flag, finalize aborts when the deterministic session ID
    /// already exists in a failed state, so re-running cannot silently
    /// purchase another proof.
    #[arg(long)]
    pub retry_failed: bool,
    /// Intermediate output root interval (checkpoint stride).
    ///
    /// Only needed when the game type has no registered implementation to
    /// read `INTERMEDIATE_BLOCK_INTERVAL` from; when it does, the flag must
    /// match that canonical value because a proof with any other stride
    /// would not verify on chain.
    #[arg(long = "intermediate-root-interval", value_name = "N")]
    pub intermediate_root_interval: Option<u64>,
    /// Prover-service RPC URL (also `BASECTL_PROVER_RPC` or config `prover_rpc`).
    #[arg(long = "prover-rpc", env = "BASECTL_PROVER_RPC", value_name = "URL")]
    pub prover_rpc: Option<Url>,
    /// `DisputeGameFactory` address (overrides config `proofs.dispute_game_factory`).
    #[arg(long = "factory", value_name = "ADDRESS")]
    pub factory: Option<Address>,
    /// L1 RPC URL (overrides config `l1_rpc`).
    #[arg(long = "l1-rpc", value_name = "URL")]
    pub l1_rpc: Option<Url>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
}

/// Dispute game locator accepted by `basectl proofs finalize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeTarget {
    /// Dispute game proxy address.
    Game(Address),
    /// L1 transaction that created the dispute game.
    CreationTransaction(B256),
}

impl FromStr for FinalizeTarget {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Ok(game) = Address::from_str(value) {
            return Ok(Self::Game(game));
        }
        if let Ok(tx_hash) = B256::from_str(value) {
            return Ok(Self::CreationTransaction(tx_hash));
        }
        Err("expected a 20-byte game address or 32-byte creation transaction hash".to_string())
    }
}

/// ZK proving backend accepted by the `basectl proofs` propose, submit, and finalize commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ZkBackendOption {
    /// Self-hosted SP1 cluster.
    Cluster,
    /// Succinct SP1 prover network (paid per proof).
    Network,
    /// Local SP1 execution statistics without proof bytes.
    DryRun,
}

impl From<ZkBackendOption> for ZkBackend {
    fn from(option: ZkBackendOption) -> Self {
        match option {
            ZkBackendOption::Cluster => Self::Cluster,
            ZkBackendOption::Network => Self::Network,
            ZkBackendOption::DryRun => Self::DryRun,
        }
    }
}

/// Proof status filter accepted by `basectl proofs list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProofStatusFilter {
    /// Proof request is queued.
    Queued,
    /// Proof request is running.
    Running,
    /// Proof request completed successfully.
    Succeeded,
    /// Proof request failed.
    Failed,
}

impl From<ProofStatusFilter> for ProofStatus {
    fn from(filter: ProofStatusFilter) -> Self {
        match filter {
            ProofStatusFilter::Queued => Self::Queued,
            ProofStatusFilter::Running => Self::Running,
            ProofStatusFilter::Succeeded => Self::Succeeded,
            ProofStatusFilter::Failed => Self::Failed,
        }
    }
}

impl ProofsCommand {
    /// Runs the selected proof operation and renders its output.
    pub async fn run(self, config: MonitoringConfig) -> Result<CommandOutcome> {
        match self.command {
            ProofsCommands::Finalize(args) => run_finalize(config, args).await,
            ProofsCommands::Status(args) => run_status(config, args).await,
            ProofsCommands::List(args) => run_list(config, args).await,
            ProofsCommands::Games(args) => run_games(config, args).await,
            ProofsCommands::Propose(args) => run_propose(config, args).await,
            ProofsCommands::Submit(args) => run_submit(config, args).await,
        }
    }
}

/// Resolves the prover-service endpoint from the CLI flag or config.
fn resolve_prover_rpc(
    config: &MonitoringConfig,
    flag: Option<Url>,
) -> Result<Url, ProofsCommandError> {
    flag.or_else(|| config.prover_rpc.clone())
        .ok_or_else(|| ProofsCommandError::MissingProverRpc { config_name: config.name.clone() })
}

/// Fetches the current state of a proof session, treating an unknown
/// session as absent.
async fn existing_proof_session(
    client: &ProofsClient,
    session_id: &str,
) -> Result<Option<GetProofResponse>, ProofsCommandError> {
    match client.proof_status(session_id).await {
        Ok(response) => Ok(Some(response)),
        Err(ProofsCommandError::Rpc { ref source, .. }) if source.is_not_found() => Ok(None),
        Err(error) => Err(error),
    }
}

async fn run_status(config: MonitoringConfig, args: ProofsStatusArgs) -> Result<CommandOutcome> {
    let ProofsStatusArgs { session_id, prover_rpc, json, raw } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    info!(
        network = %config.name,
        prover_rpc = %display_rpc_url(&endpoint),
        session_id = %session_id,
        json,
        raw,
        "fetching proof status"
    );

    let client = ProofsClient::connect(&endpoint)?;
    let response = client.proof_status(&session_id).await?;

    if raw {
        JsonOutput::print(&response)?;
        return Ok(CommandOutcome::Success);
    }

    let status = ProofsStatusJson::from_response(&config.name, &endpoint, &session_id, &response);
    if json {
        JsonOutput::print(&status)?;
    } else {
        print_status_pretty_to(&mut io::stdout().lock(), &status)?;
    }
    Ok(CommandOutcome::Success)
}

async fn run_list(config: MonitoringConfig, args: ProofsListArgs) -> Result<CommandOutcome> {
    let ProofsListArgs { status, offset, limit, prover_rpc, json } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let status_filter = status.map(ProofStatus::from);
    info!(
        network = %config.name,
        prover_rpc = %display_rpc_url(&endpoint),
        status_filter = ?status_filter,
        offset,
        limit,
        json,
        "listing proofs"
    );

    let client = ProofsClient::connect(&endpoint)?;
    let response = client.list_proofs(ListProofsRequest { offset, limit, status_filter }).await?;

    let list = ProofsListJson::from_response(
        &config.name,
        &endpoint,
        offset,
        limit,
        status_filter,
        response.total_count,
        &response.proofs,
    );
    if json {
        JsonOutput::print(&list)?;
    } else {
        print_list_pretty_to(&mut io::stdout().lock(), &list)?;
    }
    Ok(CommandOutcome::Success)
}

async fn run_games(config: MonitoringConfig, args: ProofsGamesArgs) -> Result<CommandOutcome> {
    let ProofsGamesArgs { game, limit, game_type, missing_zk, factory, l1_rpc, json } = args;
    let factory = resolve_factory(&config, factory)?;
    let l1_rpc = l1_rpc.unwrap_or_else(|| config.l1_rpc.clone());
    info!(
        network = %config.name,
        l1_rpc = %display_rpc_url(&l1_rpc),
        factory = %factory,
        game = ?game,
        limit,
        game_type = ?game_type,
        missing_zk,
        "running proofs games command"
    );

    let client = GamesClient::connect(factory, &l1_rpc);
    if let Some(game_address) = game {
        let details = client.game_details(game_address).await?;
        let details = GameDetailsJson::from_details(&config.name, &l1_rpc, factory, &details);
        if json {
            JsonOutput::print(&details)?;
        } else {
            print_game_details_pretty_to(&mut io::stdout().lock(), &details)?;
        }
        return Ok(CommandOutcome::Success);
    }

    let filter = GameListFilter { limit, game_type, missing_zk };
    let (total_games, games, search_truncated) = client.list_recent(filter).await?;
    let list = GamesListJson::from_games(
        &config.name,
        &l1_rpc,
        factory,
        total_games,
        &games,
        search_truncated,
    );
    if json {
        JsonOutput::print(&list)?;
    } else {
        print_games_list_pretty_to(&mut io::stdout().lock(), &list)?;
    }
    Ok(CommandOutcome::Success)
}

async fn run_propose(config: MonitoringConfig, args: ProofsProposeArgs) -> Result<CommandOutcome> {
    let ProofsProposeArgs {
        game,
        prover_address,
        zk_backend,
        session_id,
        retry_failed,
        intermediate_root_interval,
        wait,
        prover_rpc,
        factory,
        l1_rpc,
        yes,
        json,
    } = args;
    let zk_backend = ZkBackend::from(zk_backend);
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let factory = resolve_factory(&config, factory)?;
    let l1_rpc = l1_rpc.unwrap_or_else(|| config.l1_rpc.clone());

    let games_client = GamesClient::connect(factory, &l1_rpc);
    let details = games_client.game_details(game).await?;
    let zk_artifact_hash = games_client.proof_artifacts(game).await?.zk_artifact_hash();
    let request = ProofProposeRequest::for_game(
        &details,
        prover_address,
        zk_backend,
        zk_artifact_hash,
        session_id,
        intermediate_root_interval,
    )?;
    let prove_request = request.to_prove_request(&config.name, retry_failed);
    let prover_rpc_display = display_rpc_url(&endpoint);
    let session_id = prove_request.proof.session_id.clone();
    let client = ProofsClient::connect(&endpoint)?.with_max_wait(PROOF_MAX_WAIT);
    let existing = existing_proof_session(&client, &session_id).await?;
    let retrying_failed =
        existing.as_ref().is_some_and(|response| response.status == ProofStatus::Failed);
    if retrying_failed && !retry_failed {
        return Err(ProofsCommandError::FailedSessionRetry {
            session_id,
            message: existing
                .and_then(|response| response.error_message)
                .unwrap_or_else(|| "unknown error".to_string()),
        }
        .into());
    }
    info!(
        network = %config.name,
        prover_rpc = %prover_rpc_display,
        l1_rpc = %display_rpc_url(&l1_rpc),
        game = %game,
        prover_address = %prover_address,
        pre_state_block = request.pre_state_block,
        num_blocks = request.num_blocks,
        zk_backend = %zk_backend,
        session_id = %prove_request.proof.session_id,
        wait,
        "running proofs propose command"
    );

    let first_block = request.pre_state_block.saturating_add(1);
    let end_block = details.target_block;
    let num_blocks = request.num_blocks;
    let paid_warning = if zk_backend == ZkBackend::Network {
        " This is a PAID request billed to the worker's Succinct Network requester key."
    } else {
        ""
    };
    let retry_warning = if retrying_failed {
        " This retries the failed proof session with a NEW proof request."
    } else {
        ""
    };
    let prompt = format!(
        "Submit PLONK proposal proof request for game {game} covering blocks \
         {first_block}..={end_block} ({num_blocks} block(s), pre-state block {}) \
         bound to prover address {prover_address} via the {zk_backend} backend \
         to {prover_rpc_display}?{paid_warning}{retry_warning} [y/N] ",
        request.pre_state_block
    );
    if !Confirm::prompt_or_abort(&prompt, yes)? {
        return Ok(CommandOutcome::Success);
    }

    // The confirmation prompt can sit for a while; re-read the game so we
    // refuse to pay for a proof when it resolved or gained a ZK proof in the
    // meantime.
    games_client.ensure_accepts_zk_proof(game).await?;
    let accepted_session_id = client.submit(prove_request).await?;

    if !wait {
        let outcome =
            ProofsProposeJson::submitted(&config.name, &endpoint, &accepted_session_id, &request);
        print_propose_outcome(&outcome, json)?;
        return Ok(CommandOutcome::Success);
    }

    let response = client.wait_for_completion(&accepted_session_id).await?;
    let failed = response.status == ProofStatus::Failed;
    let outcome = ProofsProposeJson::completed(
        &config.name,
        &endpoint,
        &accepted_session_id,
        &request,
        &response,
    );
    print_propose_outcome(&outcome, json)?;
    info!(
        network = %config.name,
        prover_rpc = %prover_rpc_display,
        session_id = %accepted_session_id,
        status = %ProofOutputStatus::from(response.status),
        "proofs propose wait completed"
    );
    Ok(CommandOutcome::from_failures(failed))
}

async fn run_submit(config: MonitoringConfig, args: ProofsSubmitArgs) -> Result<CommandOutcome> {
    let ProofsSubmitArgs {
        game,
        private_key_file,
        session_id,
        zk_backend,
        intermediate_root_interval,
        wait,
        prover_rpc,
        factory,
        l1_rpc,
        yes,
        json,
    } = args;
    let zk_backend = ZkBackend::from(zk_backend);
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let factory = resolve_factory(&config, factory)?;
    let l1_rpc = l1_rpc.unwrap_or_else(|| config.l1_rpc.clone());
    let key = SubmitterKey::load(private_key_file.as_deref())?;
    let sender = key.address();

    let games_client = GamesClient::connect(factory, &l1_rpc);
    let details = games_client.game_details(game).await?;
    let zk_artifact_hash = games_client.proof_artifacts(game).await?.zk_artifact_hash();
    let derived_session = session_id.is_none();
    let session_id = ProofProposeRequest::session_id_for_game(
        &config.name,
        &details,
        sender,
        zk_backend,
        zk_artifact_hash,
        session_id,
        intermediate_root_interval,
    )?;
    let l1_rpc_display = display_rpc_url(&l1_rpc);
    let prover_rpc_display = display_rpc_url(&endpoint);
    info!(
        network = %config.name,
        prover_rpc = %prover_rpc_display,
        l1_rpc = %l1_rpc_display,
        game = %game,
        sender = %sender,
        session_id = %session_id,
        derived_session,
        wait,
        "running proofs submit command"
    );

    let first_block = details.starting_block.saturating_add(1);
    let prompt = format!(
        "Fetch the proposal proof for session {session_id} and submit \
         verifyProposalProof to game {game} covering blocks \
         {first_block}..={} ({} block(s)) from wallet {sender} via {l1_rpc_display}? \
         The submission sends an L1 transaction that costs gas. [y/N] ",
        details.target_block, details.block_interval
    );
    if !Confirm::prompt_or_abort(&prompt, yes)? {
        return Ok(CommandOutcome::Success);
    }

    let client = ProofsClient::connect(&endpoint)?.with_max_wait(PROOF_MAX_WAIT);
    let response = if wait {
        client.wait_for_completion(&session_id).await?
    } else {
        client.proof_status(&session_id).await?
    };
    let proof = SnarkPlonkProofBytes::from_response(&session_id, &response)?;

    // The proof wait can span hours; re-read the game so we refuse to spend
    // gas when it resolved or gained a ZK proof in the meantime.
    games_client.ensure_accepts_zk_proof(game).await?;

    let proof_bytes = proof.len();
    let submitter = ProposalProofSubmitter::connect(&l1_rpc, key).await?;
    let submitted = submitter.submit(game, proof).await?;
    info!(
        network = %config.name,
        game = %game,
        session_id = %session_id,
        tx_hash = %submitted.tx_hash,
        block_number = ?submitted.block_number,
        "proposal proof verified on chain"
    );

    let outcome = ProofsSubmitJson {
        network: config.name.clone(),
        l1_rpc: l1_rpc_display,
        prover_rpc: prover_rpc_display,
        session_id,
        creation_tx: None,
        game,
        sender,
        start_block: first_block,
        end_block: details.target_block,
        num_blocks: details.block_interval,
        proof_bytes,
        tx_hash: submitted.tx_hash,
        block_number: submitted.block_number,
        gas_used: submitted.gas_used,
        status: "verified",
    };
    if json {
        JsonOutput::print(&outcome)?;
    } else {
        print_submit_pretty_to(&mut io::stdout().lock(), &outcome)?;
    }
    Ok(CommandOutcome::Success)
}

async fn run_finalize(
    config: MonitoringConfig,
    args: ProofsFinalizeArgs,
) -> Result<CommandOutcome> {
    let ProofsFinalizeArgs {
        target,
        private_key_file,
        zk_backend,
        session_id,
        retry_failed,
        intermediate_root_interval,
        prover_rpc,
        factory,
        l1_rpc,
        yes,
        json,
    } = args;
    let zk_backend = ZkBackend::from(zk_backend);
    // Reject dry-run before any network work: it produces no submittable
    // proof bytes, so finalization could only fail hours later, after the
    // proof session completes.
    if zk_backend == ZkBackend::DryRun {
        return Err(ProofsCommandError::DryRunCannotFinalize.into());
    }
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let factory = resolve_factory(&config, factory)?;
    let l1_rpc = l1_rpc.unwrap_or_else(|| config.l1_rpc.clone());
    let key = SubmitterKey::load(private_key_file.as_deref())?;
    let sender = key.address();

    let games_client = GamesClient::connect(factory, &l1_rpc);
    let (game, creation_tx) = match target {
        FinalizeTarget::Game(game) => (game, None),
        FinalizeTarget::CreationTransaction(tx_hash) => {
            (games_client.game_from_creation_tx(tx_hash).await?, Some(tx_hash))
        }
    };
    let details = games_client.game_details(game).await?;
    let zk_artifact_hash = games_client.proof_artifacts(game).await?.zk_artifact_hash();
    let request = ProofProposeRequest::for_game(
        &details,
        sender,
        zk_backend,
        zk_artifact_hash,
        session_id,
        intermediate_root_interval,
    )?;
    let prove_request = request.to_prove_request(&config.name, retry_failed);
    let l1_rpc_display = display_rpc_url(&l1_rpc);
    let prover_rpc_display = display_rpc_url(&endpoint);
    info!(
        network = %config.name,
        prover_rpc = %prover_rpc_display,
        l1_rpc = %l1_rpc_display,
        creation_tx = ?creation_tx,
        game = %game,
        sender = %sender,
        pre_state_block = request.pre_state_block,
        num_blocks = request.num_blocks,
        zk_backend = %zk_backend,
        session_id = %prove_request.proof.session_id,
        "running proofs finalize command"
    );

    let session_id = prove_request.proof.session_id.clone();
    let client = ProofsClient::connect(&endpoint)?.with_max_wait(PROOF_MAX_WAIT);
    let existing = existing_proof_session(&client, &session_id).await?;
    let retrying_failed =
        existing.as_ref().is_some_and(|response| response.status == ProofStatus::Failed);
    if retrying_failed && !retry_failed {
        return Err(ProofsCommandError::FailedSessionRetry {
            session_id: session_id.clone(),
            message: existing
                .and_then(|response| response.error_message)
                .unwrap_or_else(|| "unknown error".to_string()),
        }
        .into());
    }

    let first_block = request.pre_state_block.saturating_add(1);
    let end_block = details.target_block;
    let num_blocks = request.num_blocks;
    let paid_warning = if zk_backend == ZkBackend::Network {
        " The proof is a PAID request billed to the worker's Succinct Network requester key."
    } else {
        ""
    };
    let retry_warning = if retrying_failed {
        " This retries the failed proof session with a NEW proof request."
    } else {
        ""
    };
    let prompt = format!(
        "Finalize game {game} covering blocks {first_block}..={end_block} \
         ({num_blocks} block(s)): request a PLONK proposal \
         proof via the {zk_backend} backend at {prover_rpc_display}, wait for it to complete, then \
         submit verifyProposalProof from wallet {sender} via {l1_rpc_display}?{paid_warning}{retry_warning} \
         The final step sends an L1 transaction that costs gas. [y/N] "
    );
    if !Confirm::prompt_or_abort(&prompt, yes)? {
        return Ok(CommandOutcome::Success);
    }

    // The confirmation prompt can sit for a while; re-read the game so we
    // refuse to pay for a proof when it resolved or gained a ZK proof in the
    // meantime.
    games_client.ensure_accepts_zk_proof(game).await?;
    // Wait on the server-accepted session id, as run_propose does.
    let session_id = client.submit(prove_request).await?;
    info!(
        session_id = %session_id,
        "proof request accepted; waiting for completion (re-run finalize to resume on timeout)"
    );
    let response = client.wait_for_completion(&session_id).await?;
    let proof = SnarkPlonkProofBytes::from_response(&session_id, &response)?;

    // The proof wait can span hours; re-read the game so we refuse to spend
    // gas when it resolved or gained a ZK proof in the meantime.
    games_client.ensure_accepts_zk_proof(game).await?;

    let proof_bytes = proof.len();
    let submitter = ProposalProofSubmitter::connect(&l1_rpc, key).await?;
    let submitted = submitter.submit(game, proof).await?;
    info!(
        network = %config.name,
        game = %game,
        session_id = %session_id,
        tx_hash = %submitted.tx_hash,
        block_number = ?submitted.block_number,
        "proposal proof verified on chain"
    );

    let outcome = ProofsSubmitJson {
        network: config.name.clone(),
        l1_rpc: l1_rpc_display,
        prover_rpc: prover_rpc_display,
        session_id,
        creation_tx,
        game,
        sender,
        start_block: first_block,
        end_block,
        num_blocks,
        proof_bytes,
        tx_hash: submitted.tx_hash,
        block_number: submitted.block_number,
        gas_used: submitted.gas_used,
        status: "verified",
    };
    if json {
        JsonOutput::print(&outcome)?;
    } else {
        print_submit_pretty_to(&mut io::stdout().lock(), &outcome)?;
    }
    Ok(CommandOutcome::Success)
}

/// Resolves the `DisputeGameFactory` address from the CLI flag or config.
fn resolve_factory(
    config: &MonitoringConfig,
    flag: Option<Address>,
) -> Result<Address, ProofsCommandError> {
    flag.or_else(|| config.proofs.as_ref().map(|proofs| proofs.dispute_game_factory)).ok_or_else(
        || ProofsCommandError::MissingDisputeGameFactory { config_name: config.name.clone() },
    )
}

/// Origin-only URL for output and logs — API keys in the path or userinfo must not leak.
fn display_rpc_url(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Proof request status reported by `basectl proofs` machine-readable and pretty output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofOutputStatus {
    /// The request was accepted but completion was not awaited.
    Submitted,
    /// The proof request is queued.
    Queued,
    /// The proof request is running.
    Running,
    /// The proof request succeeded.
    Succeeded,
    /// The proof request failed.
    Failed,
}

impl ProofOutputStatus {
    /// Returns the stable CLI label for this status.
    ///
    /// Delegates to `ProofsClient::status_label` so the wire-status labels have
    /// a single source of truth; only `Submitted` is local to the CLI. `Serialize`
    /// routes through here too, so JSON, pretty output, and tracing cannot drift.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Queued => ProofsClient::status_label(ProofStatus::Queued),
            Self::Running => ProofsClient::status_label(ProofStatus::Running),
            Self::Succeeded => ProofsClient::status_label(ProofStatus::Succeeded),
            Self::Failed => ProofsClient::status_label(ProofStatus::Failed),
        }
    }
}

impl fmt::Display for ProofOutputStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ProofOutputStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl From<ProofStatus> for ProofOutputStatus {
    fn from(status: ProofStatus) -> Self {
        match status {
            ProofStatus::Queued => Self::Queued,
            ProofStatus::Running => Self::Running,
            ProofStatus::Succeeded => Self::Succeeded,
            ProofStatus::Failed => Self::Failed,
        }
    }
}

/// Humanized JSON shape for a `basectl proofs propose` outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsProposeJson {
    /// Selected network name.
    pub network: String,
    /// Prover-service RPC endpoint (origin only).
    pub prover_rpc: String,
    /// Prover-service session identifier.
    pub session_id: String,
    /// Dispute game proxy address the proof targets.
    pub game: Address,
    /// L1 wallet address the proof journal commits to as proposer.
    pub prover_address: Address,
    /// First block covered by the proof.
    pub start_block: u64,
    /// Last block covered by the proof.
    pub end_block: u64,
    /// Number of consecutive L2 blocks in the proof range.
    pub num_blocks: u64,
    /// Checkpoint stride between committed intermediate output roots.
    pub intermediate_root_interval: u64,
    /// Current proof request status.
    pub status: ProofOutputStatus,
    /// Prover-service failure message, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Humanized proof result, when the proof has completed successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ProofResultJson>,
}

impl ProofsProposeJson {
    fn submitted(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        request: &ProofProposeRequest,
    ) -> Self {
        Self {
            network: network.to_string(),
            prover_rpc: display_rpc_url(prover_rpc),
            session_id: session_id.to_string(),
            game: request.game,
            prover_address: request.prover_address,
            start_block: request.pre_state_block.saturating_add(1),
            end_block: request.pre_state_block.saturating_add(request.num_blocks),
            num_blocks: request.num_blocks,
            intermediate_root_interval: request.intermediate_root_interval,
            status: ProofOutputStatus::Submitted,
            error_message: None,
            result: None,
        }
    }

    fn completed(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        request: &ProofProposeRequest,
        response: &GetProofResponse,
    ) -> Self {
        Self {
            status: response.status.into(),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
            ..Self::submitted(network, prover_rpc, session_id, request)
        }
    }
}

/// Humanized JSON shape for a `basectl proofs submit` or `proofs finalize`
/// outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsSubmitJson {
    /// Selected network name.
    pub network: String,
    /// L1 RPC endpoint the transaction was sent through (origin only).
    pub l1_rpc: String,
    /// Prover-service RPC endpoint (origin only).
    pub prover_rpc: String,
    /// Prover-service session identifier.
    pub session_id: String,
    /// L1 transaction that created the dispute game (finalize only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_tx: Option<B256>,
    /// Dispute game proxy address the proof was submitted to.
    pub game: Address,
    /// L1 wallet address that sent the transaction.
    pub sender: Address,
    /// First block covered by the proof.
    pub start_block: u64,
    /// Last block covered by the proof.
    pub end_block: u64,
    /// Number of consecutive L2 blocks in the proof range.
    pub num_blocks: u64,
    /// Size of the submitted PLONK proof in bytes.
    pub proof_bytes: usize,
    /// L1 transaction that submitted the proof to the game.
    pub tx_hash: B256,
    /// L1 block the transaction was mined in, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
    /// Gas used by the submission transaction.
    pub gas_used: u64,
    /// Submission outcome label.
    pub status: &'static str,
}

/// Humanized JSON shape for `basectl proofs status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsStatusJson {
    /// Selected network name.
    pub network: String,
    /// Prover-service RPC endpoint (origin only).
    pub prover_rpc: String,
    /// Prover-service session identifier.
    pub session_id: String,
    /// Current proof request status.
    pub status: ProofOutputStatus,
    /// Prover-service failure message, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Humanized proof result, when the proof has completed successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ProofResultJson>,
}

impl ProofsStatusJson {
    /// Builds a humanized status from a prover-service response.
    pub fn from_response(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        response: &GetProofResponse,
    ) -> Self {
        Self {
            network: network.to_string(),
            prover_rpc: display_rpc_url(prover_rpc),
            session_id: session_id.to_string(),
            status: response.status.into(),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
        }
    }
}

/// Humanized execution statistics for a dry-run ZK proof result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionStatsJson {
    /// Total RISC-V instruction cycles reported by SP1.
    pub total_instruction_cycles: u64,
    /// Total SP1 gas reported by SP1.
    pub total_sp1_gas: u64,
    /// Per-section cycle tracker values reported by the range program,
    /// sorted by section name for deterministic output.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub cycle_tracker: BTreeMap<String, u64>,
    /// Time spent generating the witness, in milliseconds.
    pub witness_generation_ms: u64,
    /// Time spent executing the SP1 range program, in milliseconds.
    pub execution_ms: u64,
}

impl ExecutionStatsJson {
    /// Builds humanized execution statistics from prover-service stats.
    pub fn from_stats(stats: &ExecutionStats) -> Self {
        Self {
            total_instruction_cycles: stats.total_instruction_cycles,
            total_sp1_gas: stats.total_sp1_gas,
            cycle_tracker: stats.cycle_tracker.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            witness_generation_ms: stats.witness_generation_ms,
            execution_ms: stats.execution_ms,
        }
    }
}

/// Humanized summary of a proof result payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofResultJson {
    /// Kind of proof returned by the prover service.
    pub proof_type: &'static str,
    /// ZK virtual machine used for a ZK proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_vm: Option<&'static str>,
    /// Trusted execution environment used for a TEE proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_kind: Option<&'static str>,
    /// Encoded proof payload size in bytes, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_bytes: Option<usize>,
    /// Local execution statistics, present for dry-run ZK results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_stats: Option<ExecutionStatsJson>,
}

impl ProofResultJson {
    /// Builds a humanized summary from a prover-service proof result.
    pub fn from_result(result: &ProofResult) -> Self {
        match result {
            ProofResult::Compressed(zk) => Self {
                proof_type: "compressed",
                zk_vm: Some(Self::zk_vm_label(zk.zk_vm)),
                tee_kind: None,
                proof_bytes: Some(zk.proof.len()),
                execution_stats: zk.execution_stats.as_ref().map(ExecutionStatsJson::from_stats),
            },
            ProofResult::SnarkPlonk(plonk) => Self {
                proof_type: "snark_plonk",
                zk_vm: Some(Self::zk_vm_label(plonk.proof.zk_vm)),
                tee_kind: None,
                proof_bytes: Some(plonk.proof.proof.len()),
                execution_stats: plonk
                    .proof
                    .execution_stats
                    .as_ref()
                    .map(ExecutionStatsJson::from_stats),
            },
            ProofResult::Tee(tee) => Self {
                proof_type: "tee",
                zk_vm: None,
                tee_kind: Some(Self::tee_kind_label(tee.tee_kind)),
                proof_bytes: None,
                execution_stats: None,
            },
        }
    }

    /// Returns the CLI label for a ZK virtual machine.
    pub const fn zk_vm_label(zk_vm: ZkVm) -> &'static str {
        match zk_vm {
            ZkVm::Sp1 => "sp1",
        }
    }

    /// Returns the CLI label for a TEE implementation.
    pub const fn tee_kind_label(tee_kind: TeeKind) -> &'static str {
        match tee_kind {
            TeeKind::AwsNitro => "aws_nitro",
        }
    }
}

/// Humanized JSON shape for `basectl proofs list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsListJson {
    /// Selected network name.
    pub network: String,
    /// Prover-service RPC endpoint (origin only).
    pub prover_rpc: String,
    /// Number of matching rows skipped by the request.
    pub offset: u64,
    /// Maximum number of rows requested.
    pub limit: u32,
    /// Requested proof status filter, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_filter: Option<ProofOutputStatus>,
    /// Total number of matching proof requests.
    pub total_count: u64,
    /// Humanized summaries returned for this page.
    pub proofs: Vec<ProofSummaryJson>,
}

impl ProofsListJson {
    /// Builds a humanized proof list from a prover-service response.
    pub fn from_response(
        network: &str,
        prover_rpc: &Url,
        offset: u64,
        limit: u32,
        status_filter: Option<ProofStatus>,
        total_count: u64,
        proofs: &[ProofSummary],
    ) -> Self {
        Self {
            network: network.to_string(),
            prover_rpc: display_rpc_url(prover_rpc),
            offset,
            limit,
            status_filter: status_filter.map(ProofOutputStatus::from),
            total_count,
            proofs: proofs.iter().map(ProofSummaryJson::from_summary).collect(),
        }
    }
}

/// Humanized JSON row for one submitted proof request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofSummaryJson {
    /// Prover-service session identifier.
    pub session_id: String,
    /// Requested proof type.
    pub proof_type: &'static str,
    /// Current proof request status.
    pub status: ProofOutputStatus,
    /// Request creation timestamp in RFC 3339 format.
    pub created_at: String,
    /// Most recent update timestamp in RFC 3339 format.
    pub updated_at: String,
    /// Completion timestamp in RFC 3339 format, when complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Prover-service failure message, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Trusted execution environment requested for a TEE proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_kind: Option<&'static str>,
    /// ZK virtual machine requested for a ZK proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_vm: Option<&'static str>,
}

impl ProofSummaryJson {
    /// Builds a humanized row from a prover-service proof summary.
    pub fn from_summary(summary: &ProofSummary) -> Self {
        Self {
            session_id: summary.session_id.clone(),
            proof_type: Self::proof_type_label(summary.proof_type),
            status: summary.status.into(),
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            completed_at: summary.completed_at.map(|at| at.to_rfc3339()),
            error_message: summary.error_message.clone(),
            tee_kind: summary.tee_kind.map(ProofResultJson::tee_kind_label),
            zk_vm: summary.zk_vm.map(ProofResultJson::zk_vm_label),
        }
    }

    /// Returns the CLI label for a proof type.
    pub const fn proof_type_label(proof_type: ProofType) -> &'static str {
        match proof_type {
            ProofType::Compressed => "compressed",
            ProofType::SnarkPlonk => "snark_plonk",
            ProofType::Tee => "tee",
        }
    }
}

/// Returns the CLI label for a dispute game status.
const fn game_status_label(status: GameStatus) -> &'static str {
    match status {
        GameStatus::InProgress => "in_progress",
        GameStatus::ChallengerWins => "challenger_wins",
        GameStatus::DefenderWins => "defender_wins",
    }
}

/// Returns `None` for the zero address, so empty prover slots serialize as absent.
fn nonzero_address(address: Address) -> Option<Address> {
    (address != Address::ZERO).then_some(address)
}

/// Formats an `expectedResolution` timestamp, mapping the unproven sentinel to `None`.
fn format_expected_resolution(timestamp: u64) -> Option<String> {
    (timestamp != EXPECTED_RESOLUTION_NEVER).then(|| Format::unix_timestamp(timestamp))
}

/// Humanized JSON shape for `basectl proofs games` (list mode).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamesListJson {
    /// Selected network name.
    pub network: String,
    /// L1 RPC endpoint the games were read from (origin only).
    pub l1_rpc: String,
    /// `DisputeGameFactory` address the games were listed from.
    pub factory: Address,
    /// Total number of games the factory has created.
    pub total_games: u64,
    /// Whether older games remain unscanned, so additional matches may exist
    /// beyond the listed games.
    pub search_truncated: bool,
    /// Listed games, newest first.
    pub games: Vec<GameSummaryJson>,
}

impl GamesListJson {
    fn from_games(
        network: &str,
        l1_rpc: &Url,
        factory: Address,
        total_games: u64,
        games: &[GameSummary],
        search_truncated: bool,
    ) -> Self {
        Self {
            network: network.to_string(),
            l1_rpc: display_rpc_url(l1_rpc),
            factory,
            total_games,
            search_truncated,
            games: games.iter().map(GameSummaryJson::from_summary).collect(),
        }
    }
}

/// Humanized JSON row for one dispute game.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSummaryJson {
    /// Game index in the factory's creation order.
    pub index: u64,
    /// Dispute game proxy address.
    pub address: Address,
    /// Game type identifier registered with the factory.
    pub game_type: u32,
    /// Current game status label.
    pub status: &'static str,
    /// Pre-state L2 block number (the covered range starts one block later).
    pub starting_block: u64,
    /// L2 block the game's root claim commits to.
    pub target_block: u64,
    /// Game creation time as a humanized UTC timestamp.
    pub created_at: String,
    /// Address that submitted the TEE proof, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_prover: Option<Address>,
    /// Address that submitted the ZK proof, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_prover: Option<Address>,
    /// Expected resolution time as a humanized UTC timestamp, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_resolution: Option<String>,
}

impl GameSummaryJson {
    fn from_summary(summary: &GameSummary) -> Self {
        Self {
            index: summary.index,
            address: summary.address,
            game_type: summary.game_type,
            status: game_status_label(summary.status),
            starting_block: summary.starting_block,
            target_block: summary.target_block,
            created_at: Format::unix_timestamp(summary.created_at),
            tee_prover: nonzero_address(summary.tee_prover),
            zk_prover: nonzero_address(summary.zk_prover),
            expected_resolution: format_expected_resolution(summary.expected_resolution),
        }
    }
}

/// Humanized JSON shape for `basectl proofs games <GAME_ADDRESS>`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDetailsJson {
    /// Selected network name.
    pub network: String,
    /// L1 RPC endpoint the game was read from (origin only).
    pub l1_rpc: String,
    /// `DisputeGameFactory` address the game was created through.
    pub factory: Address,
    /// Dispute game proxy address.
    pub address: Address,
    /// Current game status label.
    pub status: &'static str,
    /// Output root claimed for the target block.
    pub root_claim: String,
    /// Pre-state L2 block number (the covered range starts one block later).
    pub starting_block: u64,
    /// L2 block the game's root claim commits to.
    pub target_block: u64,
    /// Number of L2 blocks covered by the game.
    pub block_interval: u64,
    /// Checkpoint stride derived from the committed intermediate roots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intermediate_root_interval: Option<u64>,
    /// Number of committed intermediate output roots.
    pub intermediate_root_count: usize,
    /// L1 head hash the game was created against.
    pub l1_head: String,
    /// Parent game proxy address.
    pub parent_address: Address,
    /// Address that submitted the TEE proof, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_prover: Option<Address>,
    /// Address that submitted the ZK proof, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_prover: Option<Address>,
    /// Number of proofs submitted to the game.
    pub proof_count: u8,
    /// Game creation time as a humanized UTC timestamp.
    pub created_at: String,
    /// Expected resolution time as a humanized UTC timestamp, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_resolution: Option<String>,
    /// Index of the countered intermediate root, when the game was countered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countered_index: Option<u64>,
}

impl GameDetailsJson {
    fn from_details(network: &str, l1_rpc: &Url, factory: Address, details: &GameDetails) -> Self {
        Self {
            network: network.to_string(),
            l1_rpc: display_rpc_url(l1_rpc),
            factory,
            address: details.address,
            status: game_status_label(details.status),
            root_claim: details.root_claim.to_string(),
            starting_block: details.starting_block,
            target_block: details.target_block,
            block_interval: details.block_interval,
            intermediate_root_interval: details.intermediate_root_interval,
            intermediate_root_count: details.intermediate_root_count,
            l1_head: details.l1_head.to_string(),
            parent_address: details.parent_address,
            tee_prover: nonzero_address(details.tee_prover),
            zk_prover: nonzero_address(details.zk_prover),
            proof_count: details.proof_count,
            created_at: Format::unix_timestamp(details.created_at),
            expected_resolution: format_expected_resolution(details.expected_resolution),
            countered_index: details.countered_index,
        }
    }
}

fn print_games_list_pretty_to<W: Write>(writer: &mut W, list: &GamesListJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &list.network)
        .row("l1 rpc", &list.l1_rpc)
        .row("factory", list.factory.to_string())
        .row("total games", list.total_games.to_string())
        .row("showing", list.games.len().to_string());
    if list.search_truncated {
        table.row("warning", "search truncated; older matches may exist");
    }
    table.render(writer)?;

    if list.games.is_empty() {
        writeln!(writer, "no games")?;
        return Ok(());
    }

    writeln!(writer, "games (newest first)")?;
    for game in &list.games {
        writeln!(
            writer,
            "  [{index}] {address} blocks {start}..={end} status={status} tee={tee} zk={zk} created={created}",
            index = game.index,
            address = game.address,
            start = game.starting_block.saturating_add(1),
            end = game.target_block,
            status = game.status,
            tee = game.tee_prover.map_or_else(|| "<none>".to_string(), |a| a.to_string()),
            zk = game.zk_prover.map_or_else(|| "<none>".to_string(), |a| a.to_string()),
            created = game.created_at,
        )?;
    }
    writeln!(writer, "inspect a game with `basectl proofs games <GAME_ADDRESS>`")?;
    Ok(())
}

fn print_game_details_pretty_to<W: Write>(writer: &mut W, details: &GameDetailsJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &details.network)
        .row("l1 rpc", &details.l1_rpc)
        .row("factory", details.factory.to_string())
        .row("game", details.address.to_string())
        .row("status", details.status)
        .row("root claim", &details.root_claim)
        .row(
            "blocks",
            format!(
                "{}..={} ({} block(s))",
                details.starting_block.saturating_add(1),
                details.target_block,
                details.block_interval
            ),
        )
        .row(
            "intermediate roots",
            details.intermediate_root_interval.map_or_else(
                || details.intermediate_root_count.to_string(),
                |interval| {
                    format!("{} (every {} block(s))", details.intermediate_root_count, interval)
                },
            ),
        )
        .row("l1 head", &details.l1_head)
        .row("parent game", details.parent_address.to_string())
        .row(
            "tee prover",
            details.tee_prover.map_or_else(|| "<none>".to_string(), |a| a.to_string()),
        )
        .row("zk prover", details.zk_prover.map_or_else(|| "<none>".to_string(), |a| a.to_string()))
        .row("proof count", details.proof_count.to_string())
        .row("created", &details.created_at)
        .row(
            "expected resolution",
            details.expected_resolution.as_deref().unwrap_or("never (no proofs verified)"),
        );
    if let Some(countered_index) = details.countered_index {
        table.row("countered index", countered_index.to_string());
    }
    table.render(writer)?;
    Ok(())
}

fn print_propose_outcome(outcome: &ProofsProposeJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(outcome)?;
    } else {
        let mut stdout = io::stdout().lock();
        print_propose_pretty_to(&mut stdout, outcome)?;
    }
    Ok(())
}

fn print_propose_pretty_to<W: Write>(writer: &mut W, outcome: &ProofsProposeJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &outcome.network)
        .row("prover rpc", &outcome.prover_rpc)
        .row("session id", &outcome.session_id)
        .row("game", outcome.game.to_string())
        .row("prover address", outcome.prover_address.to_string())
        .row(
            "blocks",
            format!(
                "{}..={} ({} block(s))",
                outcome.start_block, outcome.end_block, outcome.num_blocks
            ),
        )
        .row(
            "intermediate root interval",
            format!("every {} block(s)", outcome.intermediate_root_interval),
        )
        .row("status", outcome.status.as_str());
    if let Some(error_message) = &outcome.error_message {
        table.row("error", error_message);
    }
    if let Some(result) = &outcome.result {
        append_result_rows(&mut table, result);
    }
    table.render(writer)?;
    if outcome.status == ProofOutputStatus::Submitted {
        writeln!(writer, "check progress with `basectl proofs status {}`", outcome.session_id)?;
    }
    Ok(())
}

fn print_submit_pretty_to<W: Write>(writer: &mut W, outcome: &ProofsSubmitJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &outcome.network)
        .row("l1 rpc", &outcome.l1_rpc)
        .row("prover rpc", &outcome.prover_rpc)
        .row("session id", &outcome.session_id);
    if let Some(creation_tx) = outcome.creation_tx {
        table.row("creation tx", creation_tx.to_string());
    }
    table
        .row("game", outcome.game.to_string())
        .row("sender", outcome.sender.to_string())
        .row(
            "blocks",
            format!(
                "{}..={} ({} block(s))",
                outcome.start_block, outcome.end_block, outcome.num_blocks
            ),
        )
        .row("proof size", format!("{} byte(s)", outcome.proof_bytes))
        .row("tx hash", outcome.tx_hash.to_string())
        .row(
            "block",
            outcome.block_number.map_or_else(|| "<pending>".to_string(), |n| n.to_string()),
        )
        .row("gas used", outcome.gas_used.to_string())
        .row("status", outcome.status);
    table.render(writer)?;
    writeln!(writer, "proposal proof verified on chain; the game can now resolve")?;
    Ok(())
}

fn print_status_pretty_to<W: Write>(writer: &mut W, status: &ProofsStatusJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &status.network)
        .row("prover rpc", &status.prover_rpc)
        .row("session id", &status.session_id)
        .row("status", status.status.as_str());
    if let Some(error_message) = &status.error_message {
        table.row("error", error_message);
    }
    if let Some(result) = &status.result {
        append_result_rows(&mut table, result);
    }
    table.render(writer)?;
    Ok(())
}

fn append_result_rows(table: &mut KeyValueTable, result: &ProofResultJson) {
    table.row("proof type", result.proof_type);
    if let Some(zk_vm) = result.zk_vm {
        table.row("zk vm", zk_vm);
    }
    if let Some(tee_kind) = result.tee_kind {
        table.row("tee kind", tee_kind);
    }
    if let Some(proof_bytes) = result.proof_bytes {
        table.row("proof size", format!("{proof_bytes}B"));
    }
    if let Some(stats) = &result.execution_stats {
        table.row("total cycles", stats.total_instruction_cycles.to_string());
        table.row("sp1 gas", stats.total_sp1_gas.to_string());
        table.row("witness time", format!("{}ms", stats.witness_generation_ms));
        table.row("execution time", format!("{}ms", stats.execution_ms));
    }
}

fn print_list_pretty_to<W: Write>(writer: &mut W, list: &ProofsListJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &list.network)
        .row("prover rpc", &list.prover_rpc)
        .row("total", list.total_count.to_string())
        .row(
            "showing",
            format!("{} (offset {}, limit {})", list.proofs.len(), list.offset, list.limit),
        );
    if let Some(status_filter) = list.status_filter {
        table.row("status filter", status_filter.as_str());
    }
    table.render(writer)?;

    if list.proofs.is_empty() {
        writeln!(writer, "no proofs")?;
        return Ok(());
    }

    writeln!(writer, "proofs")?;
    for proof in &list.proofs {
        writeln!(
            writer,
            "  {session} type={proof_type} status={status} created={created} completed={completed}",
            session = proof.session_id,
            proof_type = proof.proof_type,
            status = proof.status,
            created = proof.created_at,
            completed = proof.completed_at.as_deref().unwrap_or("n/a"),
        )?;
        if let Some(error_message) = &proof.error_message {
            writeln!(writer, "    error: {error_message}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::{Address, B256};
    use base_prover_service_protocol::{
        ExecutionStats, GetProofResponse, ProofResult, ProofStatus, ProofSummary, ProofType,
        SnarkPlonkProofResult, ZkBackend, ZkProofResult, ZkVm,
    };
    use url::Url;

    use super::{
        FinalizeTarget, GameDetailsJson, GamesListJson, ProofResultJson, ProofsListJson,
        ProofsProposeJson, ProofsStatusJson, ProofsSubmitJson, display_rpc_url,
        print_game_details_pretty_to, print_games_list_pretty_to, print_list_pretty_to,
        print_propose_pretty_to, print_status_pretty_to, print_submit_pretty_to,
    };
    use crate::{
        EXPECTED_RESOLUTION_NEVER, GameDetails, GameStatus, GameSummary, ProofProposeRequest,
    };

    fn prover_rpc() -> Url {
        Url::parse("http://127.0.0.1:9000").unwrap()
    }

    #[test]
    fn finalize_target_accepts_game_or_creation_transaction() {
        let game = Address::repeat_byte(0xAA);
        let tx_hash = B256::repeat_byte(0x99);

        assert_eq!(game.to_string().parse(), Ok(FinalizeTarget::Game(game)));
        assert_eq!(tx_hash.to_string().parse(), Ok(FinalizeTarget::CreationTransaction(tx_hash)));
        assert!("not-a-target".parse::<FinalizeTarget>().is_err());
    }

    fn succeeded_response() -> GetProofResponse {
        GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![0xab, 0xcd].into(),
                execution_stats: None,
            })),
        }
    }

    fn sample_summary() -> ProofSummary {
        ProofSummary {
            session_id: "session-list-1".to_string(),
            proof_type: ProofType::Compressed,
            status: ProofStatus::Failed,
            created_at: chrono_datetime(),
            updated_at: chrono_datetime(),
            completed_at: Some(chrono_datetime()),
            error_message: Some("witness generation failed".to_string()),
            tee_kind: None,
            zk_vm: Some(ZkVm::Sp1),
        }
    }

    fn chrono_datetime() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_750_000_000, 0).unwrap()
    }

    #[test]
    fn status_json_shape() {
        let status = ProofsStatusJson::from_response(
            "mainnet",
            &prover_rpc(),
            "session-1",
            &succeeded_response(),
        );
        let value = serde_json::to_value(&status).unwrap();

        assert_eq!(value["network"], "mainnet");
        assert_eq!(value["sessionId"], "session-1");
        assert_eq!(value["status"], "succeeded");
        assert_eq!(value["result"]["proofType"], "compressed");
    }

    #[test]
    fn list_json_shape() {
        let list = ProofsListJson::from_response(
            "mainnet",
            &prover_rpc(),
            0,
            50,
            Some(ProofStatus::Failed),
            1,
            &[sample_summary()],
        );
        let value = serde_json::to_value(&list).unwrap();

        assert_eq!(value["statusFilter"], "failed");
        assert_eq!(value["totalCount"], 1);
        assert_eq!(value["proofs"][0]["sessionId"], "session-list-1");
        assert_eq!(value["proofs"][0]["proofType"], "compressed");
        assert_eq!(value["proofs"][0]["status"], "failed");
        assert_eq!(value["proofs"][0]["errorMessage"], "witness generation failed");
        assert_eq!(value["proofs"][0]["zkVm"], "sp1");
        assert!(value["proofs"][0]["createdAt"].as_str().unwrap().starts_with("2025-06-15"));
    }

    fn sample_propose_request() -> ProofProposeRequest {
        ProofProposeRequest::for_game(
            &sample_game_details(),
            Address::repeat_byte(0xDD),
            ZkBackend::Network,
            B256::repeat_byte(0x33),
            Some("propose-session".to_string()),
            None,
        )
        .expect("sample game should be provable")
    }

    #[test]
    fn propose_json_shape() {
        let outcome = ProofsProposeJson::submitted(
            "mainnet",
            &prover_rpc(),
            "propose-session",
            &sample_propose_request(),
        );
        let value = serde_json::to_value(&outcome).unwrap();

        assert_eq!(value["network"], "mainnet");
        assert_eq!(value["sessionId"], "propose-session");
        assert_eq!(value["game"], serde_json::to_value(Address::repeat_byte(0xAA)).unwrap());
        assert_eq!(
            value["proverAddress"],
            serde_json::to_value(Address::repeat_byte(0xDD)).unwrap()
        );
        assert_eq!(value["startBlock"], 4001);
        assert_eq!(value["endBlock"], 5000);
        assert_eq!(value["numBlocks"], 1000);
        assert_eq!(value["intermediateRootInterval"], 100);
        assert_eq!(value["status"], "submitted");
        assert!(value.get("errorMessage").is_none());
        assert!(value.get("result").is_none());
    }

    #[test]
    fn propose_json_completed_includes_result() {
        let outcome = ProofsProposeJson::completed(
            "mainnet",
            &prover_rpc(),
            "propose-session",
            &sample_propose_request(),
            &succeeded_response(),
        );
        let value = serde_json::to_value(&outcome).unwrap();

        assert_eq!(value["status"], "succeeded");
        assert_eq!(value["result"]["proofType"], "compressed");
    }

    #[test]
    fn propose_pretty_output_smoke() {
        let outcome = ProofsProposeJson::submitted(
            "mainnet",
            &prover_rpc(),
            "propose-session",
            &sample_propose_request(),
        );
        let mut output = Vec::new();

        print_propose_pretty_to(&mut output, &outcome).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("session id"));
        assert!(rendered.contains("propose-session"));
        assert!(rendered.contains("4001..=5000 (1000 block(s))"));
        assert!(rendered.contains("every 100 block(s)"));
        assert!(rendered.contains("submitted"));
        assert!(rendered.contains("basectl proofs status propose-session"));
    }

    #[test]
    fn submit_pretty_output_smoke() {
        let outcome = ProofsSubmitJson {
            network: "mainnet".to_string(),
            l1_rpc: "http://127.0.0.1:8545/".to_string(),
            prover_rpc: prover_rpc().to_string(),
            session_id: "submit-session".to_string(),
            creation_tx: None,
            game: Address::repeat_byte(0xAA),
            sender: Address::repeat_byte(0xDD),
            start_block: 4001,
            end_block: 5000,
            num_blocks: 1000,
            proof_bytes: 1234,
            tx_hash: B256::repeat_byte(0x42),
            block_number: Some(19_000_001),
            gas_used: 321_000,
            status: "verified",
        };
        let mut output = Vec::new();

        print_submit_pretty_to(&mut output, &outcome).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("submit-session"));
        assert!(rendered.contains("4001..=5000 (1000 block(s))"));
        assert!(rendered.contains("1234 byte(s)"));
        assert!(rendered.contains(&B256::repeat_byte(0x42).to_string()));
        assert!(rendered.contains("19000001"));
        assert!(rendered.contains("verified"));
        assert!(rendered.contains("the game can now resolve"));
    }

    #[test]
    fn submit_pretty_output_handles_pending_block() {
        let outcome = ProofsSubmitJson {
            network: "mainnet".to_string(),
            l1_rpc: "http://127.0.0.1:8545/".to_string(),
            prover_rpc: prover_rpc().to_string(),
            session_id: "submit-session".to_string(),
            creation_tx: None,
            game: Address::repeat_byte(0xAA),
            sender: Address::repeat_byte(0xDD),
            start_block: 4001,
            end_block: 5000,
            num_blocks: 1000,
            proof_bytes: 1234,
            tx_hash: B256::repeat_byte(0x42),
            block_number: None,
            gas_used: 321_000,
            status: "verified",
        };
        let mut output = Vec::new();

        print_submit_pretty_to(&mut output, &outcome).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("<pending>"));
        let json = serde_json::to_value(&outcome).unwrap();
        assert!(json.get("blockNumber").is_none());
    }

    fn sample_finalize_outcome() -> ProofsSubmitJson {
        ProofsSubmitJson {
            network: "mainnet".to_string(),
            l1_rpc: "http://127.0.0.1:8545/".to_string(),
            prover_rpc: prover_rpc().to_string(),
            session_id: "finalize-session".to_string(),
            creation_tx: Some(B256::repeat_byte(0x99)),
            game: Address::repeat_byte(0xAA),
            sender: Address::repeat_byte(0xDD),
            start_block: 4001,
            end_block: 5000,
            num_blocks: 1000,
            proof_bytes: 1234,
            tx_hash: B256::repeat_byte(0x42),
            block_number: Some(19_000_001),
            gas_used: 321_000,
            status: "verified",
        }
    }

    #[test]
    fn finalize_json_shape() {
        let value = serde_json::to_value(sample_finalize_outcome()).unwrap();

        assert_eq!(value["sessionId"], "finalize-session");
        assert_eq!(value["creationTx"], serde_json::to_value(B256::repeat_byte(0x99)).unwrap());
        assert_eq!(value["startBlock"], 4001);
        assert_eq!(value["blockNumber"], 19_000_001);
        assert_eq!(value["status"], "verified");
    }

    #[test]
    fn finalize_pretty_output_smoke() {
        let mut output = Vec::new();

        print_submit_pretty_to(&mut output, &sample_finalize_outcome()).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("finalize-session"));
        assert!(rendered.contains(&B256::repeat_byte(0x99).to_string()));
        assert!(rendered.contains("4001..=5000 (1000 block(s))"));
        assert!(rendered.contains("1234 byte(s)"));
        assert!(rendered.contains(&B256::repeat_byte(0x42).to_string()));
        assert!(rendered.contains("verified"));
        assert!(rendered.contains("the game can now resolve"));
    }

    #[test]
    fn finalize_output_omits_unknown_creation_transaction() {
        let mut outcome = sample_finalize_outcome();
        outcome.creation_tx = None;
        let value = serde_json::to_value(&outcome).unwrap();
        let mut output = Vec::new();

        print_submit_pretty_to(&mut output, &outcome).unwrap();

        assert!(value.get("creationTx").is_none());
        assert!(!String::from_utf8(output).unwrap().contains("creation tx"));
    }

    #[test]
    fn status_pretty_output_includes_execution_stats() {
        let response = GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![0xab, 0xcd].into(),
                execution_stats: Some(ExecutionStats {
                    total_instruction_cycles: 12_345,
                    total_sp1_gas: 67_890,
                    cycle_tracker: HashMap::from([("execution".to_string(), 100u64)]),
                    witness_generation_ms: 5,
                    execution_ms: 7,
                }),
            })),
        };
        let status =
            ProofsStatusJson::from_response("mainnet", &prover_rpc(), "session-stats", &response);
        let mut output = Vec::new();

        print_status_pretty_to(&mut output, &status).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("total cycles"));
        assert!(rendered.contains("12345"));
        assert!(rendered.contains("sp1 gas"));
        assert!(rendered.contains("67890"));

        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["result"]["executionStats"]["totalInstructionCycles"], 12_345);
        assert_eq!(value["result"]["executionStats"]["cycleTracker"]["execution"], 100);
    }

    #[test]
    fn status_pretty_output_includes_result_rows() {
        let status = ProofsStatusJson::from_response(
            "mainnet",
            &prover_rpc(),
            "session-1",
            &succeeded_response(),
        );
        let mut output = Vec::new();

        print_status_pretty_to(&mut output, &status).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("status      succeeded"));
        assert!(rendered.contains("proof type  compressed"));
        assert!(rendered.contains("zk vm       sp1"));
        assert!(rendered.contains("proof size  2B"));
    }

    #[test]
    fn list_pretty_output_smoke() {
        let list = ProofsListJson::from_response(
            "mainnet",
            &prover_rpc(),
            0,
            50,
            None,
            1,
            &[sample_summary()],
        );
        let mut output = Vec::new();

        print_list_pretty_to(&mut output, &list).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("total       1"));
        assert!(rendered.contains("showing     1 (offset 0, limit 50)"));
        assert!(rendered.contains("session-list-1 type=compressed status=failed"));
        assert!(rendered.contains("error: witness generation failed"));
    }

    #[test]
    fn list_pretty_output_handles_empty() {
        let list = ProofsListJson::from_response("mainnet", &prover_rpc(), 0, 50, None, 0, &[]);
        let mut output = Vec::new();

        print_list_pretty_to(&mut output, &list).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("no proofs"));
    }

    #[test]
    fn snark_plonk_result_json_shape() {
        let result =
            ProofResultJson::from_result(&ProofResult::SnarkPlonk(SnarkPlonkProofResult {
                proof: ZkProofResult {
                    zk_vm: ZkVm::Sp1,
                    proof: vec![0xab, 0xcd, 0xef].into(),
                    execution_stats: None,
                },
            }));
        let value = serde_json::to_value(&result).unwrap();

        assert_eq!(value["proofType"], "snark_plonk");
        assert_eq!(value["zkVm"], "sp1");
        assert_eq!(value["proofBytes"], 3);
        assert!(value.get("teeKind").is_none());
    }

    fn sample_game_summary() -> GameSummary {
        GameSummary {
            index: 42,
            address: Address::repeat_byte(0xAA),
            game_type: 3,
            created_at: 1_750_000_000,
            status: GameStatus::InProgress,
            starting_block: 4000,
            target_block: 5000,
            tee_prover: Address::repeat_byte(0xBB),
            zk_prover: Address::ZERO,
            expected_resolution: EXPECTED_RESOLUTION_NEVER,
        }
    }

    fn sample_game_details() -> GameDetails {
        GameDetails {
            address: Address::repeat_byte(0xAA),
            status: GameStatus::InProgress,
            root_claim: B256::repeat_byte(0x11),
            starting_block: 4000,
            target_block: 5000,
            block_interval: 1000,
            intermediate_root_interval: Some(100),
            intermediate_root_count: 10,
            l1_head: B256::repeat_byte(0x22),
            parent_address: Address::repeat_byte(0xCC),
            tee_prover: Address::repeat_byte(0xBB),
            zk_prover: Address::ZERO,
            proof_count: 1,
            created_at: 1_750_000_000,
            expected_resolution: 1_750_432_000,
            countered_index: None,
        }
    }

    #[test]
    fn games_list_json_shape() {
        let factory = Address::repeat_byte(0xFF);
        let list = GamesListJson::from_games(
            "mainnet",
            &prover_rpc(),
            factory,
            100,
            &[sample_game_summary()],
            true,
        );
        let value = serde_json::to_value(&list).unwrap();

        assert_eq!(value["totalGames"], 100);
        assert_eq!(value["searchTruncated"], true);
        let game = &value["games"][0];
        assert_eq!(game["index"], 42);
        assert_eq!(game["status"], "in_progress");
        // Empty ZK slot and unproven resolution serialize as absent.
        assert!(game.get("zkProver").is_none());
        assert!(game.get("expectedResolution").is_none());
        assert!(game.get("teeProver").is_some());

        // An untruncated search serializes `searchTruncated` as `false`.
        let full = GamesListJson::from_games("mainnet", &prover_rpc(), factory, 100, &[], false);
        assert_eq!(serde_json::to_value(&full).unwrap()["searchTruncated"], false);
    }

    #[test]
    fn games_list_pretty_output() {
        let factory = Address::repeat_byte(0xFF);
        let list = GamesListJson::from_games(
            "mainnet",
            &prover_rpc(),
            factory,
            100,
            &[sample_game_summary()],
            false,
        );
        let mut output = Vec::new();

        print_games_list_pretty_to(&mut output, &list).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("total games  100"));
        assert!(rendered.contains("[42]"));
        assert!(rendered.contains("blocks 4001..=5000"));
        assert!(rendered.contains("zk=<none>"));
        assert!(rendered.contains("status=in_progress"));
        assert!(!rendered.contains("older matches may exist"));
    }

    #[test]
    fn games_list_pretty_output_handles_empty() {
        let factory = Address::repeat_byte(0xFF);
        let list = GamesListJson::from_games("mainnet", &prover_rpc(), factory, 0, &[], false);
        let mut output = Vec::new();

        print_games_list_pretty_to(&mut output, &list).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("no games"));
    }

    #[test]
    fn games_list_pretty_output_warns_when_truncated() {
        let factory = Address::repeat_byte(0xFF);
        let list = GamesListJson::from_games(
            "mainnet",
            &prover_rpc(),
            factory,
            500,
            &[sample_game_summary()],
            true,
        );
        let mut output = Vec::new();

        print_games_list_pretty_to(&mut output, &list).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("older matches may exist"));
    }

    #[test]
    fn game_details_json_shape() {
        let factory = Address::repeat_byte(0xFF);
        let details = GameDetailsJson::from_details(
            "mainnet",
            &prover_rpc(),
            factory,
            &sample_game_details(),
        );
        let value = serde_json::to_value(&details).unwrap();

        assert_eq!(value["status"], "in_progress");
        assert_eq!(value["intermediateRootInterval"], 100);
        assert!(value.get("zkProver").is_none());
        assert!(value.get("counteredIndex").is_none());
        assert!(value["expectedResolution"].is_string());
    }

    #[test]
    fn game_details_pretty_output() {
        let factory = Address::repeat_byte(0xFF);
        let details = GameDetailsJson::from_details(
            "mainnet",
            &prover_rpc(),
            factory,
            &sample_game_details(),
        );
        let mut output = Vec::new();

        print_game_details_pretty_to(&mut output, &details).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("blocks"));
        assert!(rendered.contains("4001..=5000 (1000 block(s))"));
        assert!(rendered.contains("10 (every 100 block(s))"));
        assert!(rendered.contains("zk prover"));
        assert!(rendered.contains("<none>"));
    }

    #[test]
    fn game_details_pretty_output_unproven_resolution() {
        let factory = Address::repeat_byte(0xFF);
        let details =
            GameDetails { expected_resolution: EXPECTED_RESOLUTION_NEVER, ..sample_game_details() };
        let details = GameDetailsJson::from_details("mainnet", &prover_rpc(), factory, &details);
        let mut output = Vec::new();

        print_game_details_pretty_to(&mut output, &details).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("never (no proofs verified)"));
    }

    #[test]
    fn display_rpc_url_strips_path_and_userinfo() {
        let url = Url::parse("https://user:secret@l1.example/v1/api-key-123?q=1").unwrap();

        assert_eq!(display_rpc_url(&url), "https://l1.example");
    }

    #[test]
    fn game_details_json_redacts_l1_rpc_credentials() {
        let factory = Address::repeat_byte(0xFF);
        let l1_rpc = Url::parse("https://user:secret@l1.example/v1/api-key-123").unwrap();
        let details =
            GameDetailsJson::from_details("mainnet", &l1_rpc, factory, &sample_game_details());

        assert_eq!(details.l1_rpc, "https://l1.example");
    }
}
