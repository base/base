//! Implementation of the `basectl proofs` command group.

use std::{
    fmt,
    io::{self, Write},
};

use alloy_primitives::{Address, B256};
use anyhow::Result;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameStatus, ProofProtocolDescriptor, ProofScheduleKind,
};
use base_prover_service_protocol::{
    GetProofResponse, ListProofsRequest, ProofResult, ProofStatus, ProofSummary, ProofType,
    TeeKind, ZkVm,
};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Serialize, Serializer};
use tracing::{info, warn};
use url::Url;

use crate::{
    CommandOutcome, Confirm, JsonOutput, KeyValueTable, MonitoringConfig, ProofFinalizeRequest,
    ProofsClient, ProofsCommandError,
};

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
    /// Submit a compressed ZK proof request for a block range to speed up finality.
    Finalize(ProofsFinalizeArgs),
    /// Show status and result data for a submitted proof request.
    Status(ProofsStatusArgs),
    /// List submitted proof requests.
    List(ProofsListArgs),
    /// Print the proof capability fingerprint of historical dispute games.
    ///
    /// Reads the immutable commitments each game proxy exposes and derives the fingerprint the
    /// challenger routes on, so `--proof-protocol-version <fingerprint>=<version>` mappings can be
    /// written from observed state rather than guessed. Read-only.
    Protocol(ProofsProtocolArgs),
}

/// Flags for `basectl proofs protocol`.
#[derive(Debug, Args)]
pub struct ProofsProtocolArgs {
    /// L1 RPC endpoint used to read game state.
    #[arg(long = "l1-rpc", env = "BASECTL_L1_RPC", value_name = "URL")]
    pub l1_rpc: Url,
    /// `DisputeGameFactory` address to enumerate games from.
    #[arg(long = "factory", value_name = "ADDRESS", required_unless_present = "game")]
    pub factory: Option<Address>,
    /// First factory index to read. Defaults to 0.
    #[arg(long = "from", value_name = "INDEX", default_value_t = 0)]
    pub from: u64,
    /// Last factory index to read. Defaults to the newest game.
    #[arg(long = "to", value_name = "INDEX")]
    pub to: Option<u64>,
    /// Explicit game proxy address. Repeatable; skips factory enumeration.
    #[arg(long = "game", value_name = "ADDRESS")]
    pub game: Vec<Address>,
    /// Emit JSON instead of a table.
    #[arg(long = "json")]
    pub json: bool,
}

/// Flags for `basectl proofs finalize`.
#[derive(Debug, Args)]
pub struct ProofsFinalizeArgs {
    /// First L2 block number to prove.
    #[arg(value_name = "START_BLOCK")]
    pub start_block: u64,
    /// Number of consecutive L2 blocks to prove.
    #[arg(value_name = "NUM_BLOCKS", value_parser = clap::value_parser!(u64).range(1..))]
    pub num_blocks: u64,
    /// Explicit proof session ID (prover-service idempotency key).
    ///
    /// If omitted, basectl derives a deterministic session ID from the
    /// network name and block range, so re-running the same command resolves
    /// to the existing prover-service session instead of enqueueing a
    /// duplicate proof.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
    /// L1 head hash used for witness generation.
    ///
    /// If omitted, the prover service picks one.
    #[arg(long = "l1-head", value_name = "HASH")]
    pub l1_head: Option<B256>,
    /// Sequencing window passed to the prover.
    #[arg(long = "sequence-window", value_name = "N")]
    pub sequence_window: Option<u64>,
    /// Intermediate output root interval passed to the prover.
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
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Emit a structured JSON action outcome instead of pretty text.
    #[arg(long, requires = "yes")]
    pub json: bool,
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
            ProofsCommands::Protocol(args) => run_protocol(args).await,
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

async fn run_finalize(
    config: MonitoringConfig,
    args: ProofsFinalizeArgs,
) -> Result<CommandOutcome> {
    let ProofsFinalizeArgs {
        start_block,
        num_blocks,
        session_id,
        l1_head,
        sequence_window,
        intermediate_root_interval,
        wait,
        prover_rpc,
        yes,
        json,
    } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let request = ProofFinalizeRequest {
        start_block,
        num_blocks,
        session_id,
        l1_head,
        sequence_window,
        intermediate_root_interval,
    };
    let request = request.to_prove_request(&config.name);
    let end_block = start_block.saturating_add(num_blocks.saturating_sub(1));
    info!(
        network = %config.name,
        prover_rpc = %endpoint,
        start_block,
        end_block,
        session_id = %request.proof.session_id,
        wait,
        "running proofs finalize command"
    );

    let prompt = format!(
        "Submit compressed ZK proof request for blocks {start_block}..={end_block} \
         ({num_blocks} block(s)) to {endpoint}? [y/N] "
    );
    if !Confirm::prompt_or_abort(&prompt, yes)? {
        return Ok(CommandOutcome::Success);
    }

    let client = ProofsClient::connect(&endpoint)?;
    let accepted_session_id = client.submit(request).await?;

    if !wait {
        let outcome = ProofsFinalizeJson::submitted(
            &config.name,
            &endpoint,
            &accepted_session_id,
            start_block,
            num_blocks,
        );
        print_finalize_outcome(&outcome, json)?;
        return Ok(CommandOutcome::Success);
    }

    let response = client.wait_for_completion(&accepted_session_id).await?;
    let failed = response.status == ProofStatus::Failed;
    let outcome = ProofsFinalizeJson::completed(
        &config.name,
        &endpoint,
        &accepted_session_id,
        start_block,
        num_blocks,
        &response,
    );
    print_finalize_outcome(&outcome, json)?;
    info!(
        network = %config.name,
        prover_rpc = %endpoint,
        session_id = %accepted_session_id,
        status = %ProofOutputStatus::from(response.status),
        "proofs finalize wait completed"
    );
    Ok(CommandOutcome::from_failures(failed))
}

async fn run_status(config: MonitoringConfig, args: ProofsStatusArgs) -> Result<CommandOutcome> {
    let ProofsStatusArgs { session_id, prover_rpc, json, raw } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    info!(
        network = %config.name,
        prover_rpc = %endpoint,
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

/// One game's capability descriptor, flattened for display.
#[derive(Debug, Serialize)]
struct GameProtocolRow {
    /// Factory index, absent when the game was named explicitly.
    index: Option<u64>,
    /// Game proxy address.
    game: Address,
    /// Current onchain game status.
    status: &'static str,
    /// Journal schedule era.
    era: &'static str,
    /// Rollup configuration hash committed by both journal types.
    config_hash: B256,
    /// Nitro enclave image hash committed by TEE journals.
    tee_image_hash: B256,
    /// SP1 range verification key committed by ZK journals.
    zk_range_hash: B256,
    /// SP1 aggregation verification key used by the ZK verifier.
    zk_aggregate_hash: B256,
    /// Canonical fingerprint the challenger maps to a routing version.
    fingerprint: B256,
}

impl GameProtocolRow {
    fn new(
        index: Option<u64>,
        game: Address,
        status: GameStatus,
        descriptor: &ProofProtocolDescriptor,
    ) -> Self {
        Self {
            index,
            game,
            status: match status {
                GameStatus::InProgress => "in-progress",
                GameStatus::ChallengerWins => "challenger-wins",
                GameStatus::DefenderWins => "defender-wins",
            },
            era: match descriptor.schedule_kind {
                ProofScheduleKind::None => "no-schedule",
                ProofScheduleKind::Full => "full-schedule",
                ProofScheduleKind::Activated => "activated-prefix",
            },
            config_hash: descriptor.config_hash,
            tee_image_hash: descriptor.tee_image_hash,
            zk_range_hash: descriptor.zk_range_hash,
            zk_aggregate_hash: descriptor.zk_aggregate_hash,
            fingerprint: descriptor.fingerprint(),
        }
    }
}

async fn run_protocol(args: ProofsProtocolArgs) -> Result<CommandOutcome> {
    let ProofsProtocolArgs { l1_rpc, factory, from, to, game, json } = args;
    let verifier = AggregateVerifierContractClient::new(l1_rpc.clone())?;

    let targets: Vec<(Option<u64>, Address)> = if game.is_empty() {
        let factory_address =
            factory.ok_or_else(|| anyhow::anyhow!("--factory or --game is required"))?;
        let factory_client = DisputeGameFactoryContractClient::new(factory_address, l1_rpc)?;
        let count = factory_client.game_count().await?;
        let end = to.unwrap_or_else(|| count.saturating_sub(1));
        info!(factory = %factory_address, from, end, count, "enumerating dispute games");

        let mut targets = Vec::new();
        if count > 0 {
            for index in from..=end.min(count - 1) {
                targets.push((Some(index), factory_client.game_at_index(index).await?.proxy));
            }
        }
        targets
    } else {
        game.into_iter().map(|address| (None, address)).collect()
    };

    let mut rows = Vec::with_capacity(targets.len());
    let mut unreadable = 0;
    for (index, address) in targets {
        // One unreadable game must not hide the rest: an era this build cannot classify is
        // exactly what the operator needs to see.
        match futures::try_join!(
            verifier.status(address),
            verifier.proof_protocol_descriptor(address)
        ) {
            Ok((status, descriptor)) => {
                rows.push(GameProtocolRow::new(index, address, status, &descriptor));
            }
            Err(error) => {
                unreadable += 1;
                warn!(game = %address, error = %error, "skipping unreadable game");
            }
        }
    }

    if json {
        JsonOutput::print(&rows)?;
    } else {
        print_protocol_pretty_to(&mut io::stdout().lock(), &rows)?;
    }

    if unreadable > 0 {
        anyhow::bail!("{unreadable} game(s) could not be classified; fingerprint dump incomplete");
    }

    Ok(CommandOutcome::Success)
}

fn print_protocol_pretty_to(out: &mut impl Write, rows: &[GameProtocolRow]) -> io::Result<()> {
    for row in rows {
        let mut table = KeyValueTable::new();
        if let Some(index) = row.index {
            table.row("Index", index.to_string());
        }
        table.row("Game", row.game.to_string());
        table.row("Status", row.status.to_owned());
        table.row("Era", row.era.to_owned());
        table.row("Config hash", row.config_hash.to_string());
        table.row("TEE image hash", row.tee_image_hash.to_string());
        table.row("ZK range hash", row.zk_range_hash.to_string());
        table.row("ZK aggregate hash", row.zk_aggregate_hash.to_string());
        table.row("Fingerprint", row.fingerprint.to_string());
        table.render(out)?;
        writeln!(out)?;
    }

    // Resolved games no longer need workers. Build the mapping only from games the challenger may
    // still need to prove, while keeping resolved rows above as useful history.
    let mut distinct: Vec<B256> =
        rows.iter().filter(|row| row.status == "in-progress").map(|row| row.fingerprint).collect();
    distinct.sort_unstable();
    distinct.dedup();

    writeln!(
        out,
        "{} in-progress game(s), {} distinct capability fingerprint(s):",
        rows.iter().filter(|row| row.status == "in-progress").count(),
        distinct.len()
    )?;
    for fingerprint in distinct {
        let games = rows
            .iter()
            .filter(|row| row.status == "in-progress" && row.fingerprint == fingerprint)
            .count();
        writeln!(out, "  {fingerprint}  ({games} game(s))  -> assign a version")?;
    }

    Ok(())
}

async fn run_list(config: MonitoringConfig, args: ProofsListArgs) -> Result<CommandOutcome> {
    let ProofsListArgs { status, offset, limit, prover_rpc, json } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let status_filter = status.map(ProofStatus::from);
    info!(
        network = %config.name,
        prover_rpc = %endpoint,
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

/// Humanized JSON shape for a `basectl proofs finalize` outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsFinalizeJson {
    /// Selected network name.
    pub network: String,
    /// Prover-service RPC endpoint.
    pub prover_rpc: String,
    /// Prover-service session identifier.
    pub session_id: String,
    /// First L2 block in the proof range.
    pub start_block: u64,
    /// Last L2 block in the inclusive proof range.
    pub end_block: u64,
    /// Number of consecutive L2 blocks in the proof range.
    pub num_blocks: u64,
    /// Current proof request status.
    pub status: ProofOutputStatus,
    /// Prover-service failure message, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Humanized proof result, when the proof has completed successfully.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ProofResultJson>,
}

impl ProofsFinalizeJson {
    /// Builds the outcome for an accepted proof request that is not being awaited.
    pub fn submitted(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        start_block: u64,
        num_blocks: u64,
    ) -> Self {
        Self {
            network: network.to_string(),
            prover_rpc: prover_rpc.to_string(),
            session_id: session_id.to_string(),
            start_block,
            end_block: start_block.saturating_add(num_blocks.saturating_sub(1)),
            num_blocks,
            status: ProofOutputStatus::Submitted,
            error_message: None,
            result: None,
        }
    }

    /// Builds the outcome for a proof request after completion polling.
    pub fn completed(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        start_block: u64,
        num_blocks: u64,
        response: &GetProofResponse,
    ) -> Self {
        Self {
            status: response.status.into(),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
            ..Self::submitted(network, prover_rpc, session_id, start_block, num_blocks)
        }
    }
}

/// Humanized JSON shape for `basectl proofs status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsStatusJson {
    /// Selected network name.
    pub network: String,
    /// Prover-service RPC endpoint.
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
            prover_rpc: prover_rpc.to_string(),
            session_id: session_id.to_string(),
            status: response.status.into(),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
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
            },
            ProofResult::SnarkPlonk(plonk) => Self {
                proof_type: "snark_plonk",
                zk_vm: Some(Self::zk_vm_label(plonk.proof.zk_vm)),
                tee_kind: None,
                proof_bytes: Some(plonk.proof.proof.len()),
            },
            ProofResult::Tee(tee) => Self {
                proof_type: "tee",
                zk_vm: None,
                tee_kind: Some(Self::tee_kind_label(tee.tee_kind)),
                proof_bytes: None,
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
    /// Prover-service RPC endpoint.
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
            prover_rpc: prover_rpc.to_string(),
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

fn print_finalize_outcome(outcome: &ProofsFinalizeJson, json: bool) -> Result<()> {
    if json {
        JsonOutput::print(outcome)?;
    } else {
        let mut stdout = io::stdout().lock();
        print_finalize_pretty_to(&mut stdout, outcome)?;
    }
    Ok(())
}

fn print_finalize_pretty_to<W: Write>(writer: &mut W, outcome: &ProofsFinalizeJson) -> Result<()> {
    let mut table = KeyValueTable::new();
    table
        .row("network", &outcome.network)
        .row("prover rpc", &outcome.prover_rpc)
        .row("session id", &outcome.session_id)
        .row(
            "blocks",
            format!(
                "{}..={} ({} block(s))",
                outcome.start_block, outcome.end_block, outcome.num_blocks
            ),
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
    use alloy_primitives::{Address, B256};
    use base_proof_contracts::{GameStatus, ProofProtocolDescriptor, ProofScheduleKind};

    use super::{GameProtocolRow, print_protocol_pretty_to};

    fn descriptor(tee_image_hash: u8) -> ProofProtocolDescriptor {
        ProofProtocolDescriptor {
            schedule_kind: ProofScheduleKind::Activated,
            schedule_id: B256::ZERO,
            config_hash: B256::repeat_byte(1),
            tee_image_hash: B256::repeat_byte(tee_image_hash),
            zk_range_hash: B256::repeat_byte(3),
            zk_aggregate_hash: B256::repeat_byte(4),
        }
    }

    /// Games sharing a capability collapse to one mapping line; a differing commitment does not.
    #[test]
    fn protocol_summary_groups_games_by_fingerprint() {
        let rows = vec![
            GameProtocolRow::new(
                Some(0),
                Address::repeat_byte(0xa1),
                GameStatus::InProgress,
                &descriptor(2),
            ),
            GameProtocolRow::new(
                Some(1),
                Address::repeat_byte(0xa2),
                GameStatus::InProgress,
                &descriptor(2),
            ),
            GameProtocolRow::new(
                Some(2),
                Address::repeat_byte(0xa3),
                GameStatus::DefenderWins,
                &descriptor(9),
            ),
        ];

        let mut out = Vec::new();
        print_protocol_pretty_to(&mut out, &rows).expect("summary should render");
        let rendered = String::from_utf8(out).expect("summary should be utf8");

        assert!(rendered.contains("2 in-progress game(s), 1 distinct capability fingerprint(s)"));
        assert!(rendered.contains("(2 game(s))"), "shared capability should group");
        assert!(rendered.contains("defender-wins"), "resolved games should remain visible");
        assert!(rendered.contains("activated-prefix"));
    }

    use base_prover_service_protocol::{
        GetProofResponse, ProofResult, ProofStatus, ProofSummary, ProofType, SnarkPlonkProofResult,
        ZkProofResult, ZkVm,
    };
    use url::Url;

    use super::{
        ProofResultJson, ProofsFinalizeJson, ProofsListJson, ProofsStatusJson,
        print_finalize_pretty_to, print_list_pretty_to, print_status_pretty_to,
    };

    fn prover_rpc() -> Url {
        Url::parse("http://127.0.0.1:9000").unwrap()
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
    fn finalize_submitted_json_shape() {
        let outcome = ProofsFinalizeJson::submitted("mainnet", &prover_rpc(), "session-1", 100, 5);
        let value = serde_json::to_value(&outcome).unwrap();

        assert_eq!(value["network"], "mainnet");
        assert_eq!(value["proverRpc"], "http://127.0.0.1:9000/");
        assert_eq!(value["sessionId"], "session-1");
        assert_eq!(value["startBlock"], 100);
        assert_eq!(value["endBlock"], 104);
        assert_eq!(value["numBlocks"], 5);
        assert_eq!(value["status"], "submitted");
        assert!(value.get("errorMessage").is_none());
        assert!(value.get("result").is_none());
    }

    #[test]
    fn finalize_completed_json_includes_result() {
        let outcome = ProofsFinalizeJson::completed(
            "mainnet",
            &prover_rpc(),
            "session-1",
            100,
            5,
            &succeeded_response(),
        );
        let value = serde_json::to_value(&outcome).unwrap();

        assert_eq!(value["status"], "succeeded");
        assert_eq!(value["result"]["proofType"], "compressed");
        assert_eq!(value["result"]["zkVm"], "sp1");
        assert_eq!(value["result"]["proofBytes"], 2);
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

    #[test]
    fn finalize_pretty_output_smoke() {
        let outcome = ProofsFinalizeJson::submitted("mainnet", &prover_rpc(), "session-1", 100, 5);
        let mut output = Vec::new();

        print_finalize_pretty_to(&mut output, &outcome).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("network     mainnet"));
        assert!(rendered.contains("session id  session-1"));
        assert!(rendered.contains("blocks      100..=104 (5 block(s))"));
        assert!(rendered.contains("status      submitted"));
        assert!(rendered.contains("basectl proofs status session-1"));
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
}
