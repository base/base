//! Implementation of the `basectl proofs` command group.

use std::{
    fmt,
    io::{self, Write},
};

use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use basectl_cli::{
    JsonOutput, MonitoringConfig, OnchainProofsReport, ProofsClient, ProofsCommandError,
    ProofsConfig, ProofsGapReport, ProofsJobListRequest, ProofsJobStatus, ProofsProposal,
    ProverProofSummary, ProverProofsPage, TimestampJson, format_unix_timestamp,
};
use serde::Serialize;
use url::Url;

use crate::{
    cli::{ProofsCommands, ProofsListArgs},
    helpers::CommandOutcome,
};

/// Runs the `basectl proofs` command group.
pub(crate) async fn run(
    config: MonitoringConfig,
    command: ProofsCommands,
) -> Result<CommandOutcome> {
    match command {
        ProofsCommands::List(args) => run_list(config, args).await,
    }
}

async fn run_list(config: MonitoringConfig, args: ProofsListArgs) -> Result<CommandOutcome> {
    let request = ProofsListRequest::resolve(config, args)?;
    let (onchain, prover) = tokio::join!(
        async {
            match &request.onchain {
                Some(source) => SourceResult::from_result(
                    ProofsClient::fetch_onchain_report(
                        &source.contracts,
                        &source.l1_rpc,
                        &source.l2_rpc,
                        request.scan_window,
                        request.limit,
                    )
                    .await,
                ),
                None => SourceResult::Skipped("no proof contract addresses configured".to_string()),
            }
        },
        async {
            match &request.prover_url {
                Some(prover_url) => SourceResult::from_result(
                    ProofsClient::list_prover_jobs(
                        prover_url,
                        ProofsJobListRequest {
                            offset: request.offset,
                            limit: request.limit,
                            status: request.status,
                        },
                    )
                    .await,
                ),
                None => SourceResult::Skipped("no prover-service URL configured".to_string()),
            }
        },
    );

    let report = ProofsListReport { request, onchain, prover };

    let has_failures = if report.request.json {
        let json = ProofsListJson::from_report(&report);
        let has_failures = json.has_error();
        JsonOutput::print(&json)?;
        has_failures
    } else {
        print_pretty(&report)?;
        report.onchain.has_error() || report.prover.has_error()
    };

    Ok(CommandOutcome::from_failures(has_failures))
}

#[derive(Debug, Clone)]
struct ProofsListRequest {
    network: String,
    onchain: Option<OnchainSource>,
    prover_url: Option<Url>,
    status: Option<ProofsJobStatus>,
    limit: u32,
    offset: u64,
    scan_window: u64,
    json: bool,
}

impl ProofsListRequest {
    fn resolve(config: MonitoringConfig, args: ProofsListArgs) -> Result<Self, ProofsCommandError> {
        if !(1..=ProofsClient::MAX_ONCHAIN_REPORT_LIMIT).contains(&args.limit) {
            return Err(ProofsCommandError::LimitOutOfRange { limit: args.limit });
        }
        if !(1..=ProofsClient::MAX_ONCHAIN_SCAN_WINDOW).contains(&args.scan_window) {
            return Err(ProofsCommandError::ScanWindowOutOfRange { scan_window: args.scan_window });
        }
        if args.dispute_game_factory.is_some() != args.anchor_state_registry.is_some() {
            return Err(ProofsCommandError::PartialContractOverride);
        }

        let config_proofs = config.proofs.clone();
        let onchain = resolve_onchain_source(&config, &args, config_proofs.as_ref())?;
        let prover_url = args
            .prover_url
            .clone()
            .or_else(|| config_proofs.as_ref().and_then(|proofs| proofs.prover_url.clone()));

        if args.status.is_some() && prover_url.is_none() {
            return Err(ProofsCommandError::MissingProverSource { flag: "--status" });
        }
        if args.offset > 0 && prover_url.is_none() {
            return Err(ProofsCommandError::MissingProverSource { flag: "--offset" });
        }
        if onchain.is_none() && prover_url.is_none() {
            return Err(ProofsCommandError::MissingSource { config_name: config.name });
        }

        Ok(Self {
            network: config.name,
            onchain,
            prover_url,
            status: args.status,
            limit: args.limit,
            offset: args.offset,
            scan_window: args.scan_window,
            json: args.json,
        })
    }
}

#[derive(Debug, Clone)]
struct OnchainSource {
    l1_rpc: Url,
    l2_rpc: Url,
    contracts: ProofsConfig,
}

#[derive(Debug, Clone)]
struct ProofsListReport {
    request: ProofsListRequest,
    onchain: SourceResult<OnchainProofsReport>,
    prover: SourceResult<ProverProofsPage>,
}

#[derive(Debug, Clone)]
enum SourceResult<T> {
    Available(T),
    Skipped(String),
    Error(String),
}

impl<T> SourceResult<T> {
    fn from_result(result: Result<T>) -> Self {
        match result {
            Ok(value) => Self::Available(value),
            Err(error) => Self::Error(format!("{error:#}")),
        }
    }

    const fn has_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

fn resolve_onchain_source(
    config: &MonitoringConfig,
    args: &ProofsListArgs,
    config_proofs: Option<&ProofsConfig>,
) -> Result<Option<OnchainSource>, ProofsCommandError> {
    let contracts = match (args.dispute_game_factory, args.anchor_state_registry) {
        (Some(dispute_game_factory), Some(anchor_state_registry)) => Some(ProofsConfig {
            dispute_game_factory,
            anchor_state_registry,
            prover_url: config_proofs.and_then(|proofs| proofs.prover_url.clone()),
        }),
        (None, None) => config_proofs.cloned(),
        _ => unreachable!("contract override validation requires both proof contracts or neither"),
    };

    Ok(contracts.map(|contracts| OnchainSource {
        l1_rpc: args.l1_rpc.clone().unwrap_or_else(|| config.l1_rpc.clone()),
        l2_rpc: args.l2_rpc.clone().unwrap_or_else(|| config.rpc.clone()),
        contracts,
    }))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsListJson {
    network: String,
    inputs: ProofsInputsJson,
    onchain: SourceJson<OnchainProofsJson>,
    prover: SourceJson<ProverProofsJson>,
}

impl ProofsListJson {
    fn from_report(report: &ProofsListReport) -> Self {
        Self {
            network: report.request.network.clone(),
            inputs: ProofsInputsJson::from_request(&report.request),
            onchain: SourceJson::from_source(&report.onchain, OnchainProofsJson::from_report),
            prover: SourceJson::try_from_source(&report.prover, ProverProofsJson::try_from_page),
        }
    }

    const fn has_error(&self) -> bool {
        self.onchain.has_error() || self.prover.has_error()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsInputsJson {
    limit: u32,
    offset: u64,
    scan_window: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<ProofsJobStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    l1_rpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    l2_rpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispute_game_factory: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_state_registry: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prover_url: Option<String>,
}

impl ProofsInputsJson {
    fn from_request(request: &ProofsListRequest) -> Self {
        let onchain = request.onchain.as_ref();
        Self {
            limit: request.limit,
            offset: request.offset,
            scan_window: request.scan_window,
            status: request.status,
            l1_rpc: onchain.map(|source| source.l1_rpc.to_string()),
            l2_rpc: onchain.map(|source| source.l2_rpc.to_string()),
            dispute_game_factory: onchain.map(|source| source.contracts.dispute_game_factory),
            anchor_state_registry: onchain.map(|source| source.contracts.anchor_state_registry),
            prover_url: request.prover_url.as_ref().map(ToString::to_string),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceStatusJson {
    Available,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceJson<T> {
    status: SourceStatusJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
}

impl<T> SourceJson<T> {
    fn from_source<U>(source: &SourceResult<U>, convert: impl FnOnce(&U) -> T) -> Self {
        match source {
            SourceResult::Available(value) => Self {
                status: SourceStatusJson::Available,
                reason: None,
                error: None,
                data: Some(convert(value)),
            },
            SourceResult::Skipped(reason) => Self {
                status: SourceStatusJson::Skipped,
                reason: Some(reason.clone()),
                error: None,
                data: None,
            },
            SourceResult::Error(error) => Self {
                status: SourceStatusJson::Error,
                reason: None,
                error: Some(error.clone()),
                data: None,
            },
        }
    }

    fn try_from_source<U>(source: &SourceResult<U>, convert: impl FnOnce(&U) -> Result<T>) -> Self {
        match source {
            SourceResult::Available(value) => match convert(value) {
                Ok(data) => Self {
                    status: SourceStatusJson::Available,
                    reason: None,
                    error: None,
                    data: Some(data),
                },
                Err(error) => Self {
                    status: SourceStatusJson::Error,
                    reason: None,
                    error: Some(format!("{error:#}")),
                    data: None,
                },
            },
            SourceResult::Skipped(reason) => Self {
                status: SourceStatusJson::Skipped,
                reason: Some(reason.clone()),
                error: None,
                data: None,
            },
            SourceResult::Error(error) => Self {
                status: SourceStatusJson::Error,
                reason: None,
                error: Some(error.clone()),
                data: None,
            },
        }
    }

    const fn has_error(&self) -> bool {
        matches!(self.status, SourceStatusJson::Error)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OnchainProofsJson {
    l1_block: Option<u64>,
    l2_latest_block: Option<u64>,
    l2_safe_block: Option<u64>,
    l2_finalized_block: Option<u64>,
    respected_game_type: Option<u32>,
    system_paused: Option<bool>,
    total_games: Option<u64>,
    anchor_l2_block: Option<u64>,
    anchor_root: Option<B256>,
    proposals: Vec<ProofsProposalJson>,
    gaps: ProofsGapReport,
}

impl OnchainProofsJson {
    fn from_report(report: &OnchainProofsReport) -> Self {
        Self {
            l1_block: report.l1_block,
            l2_latest_block: report.l2_latest_block,
            l2_safe_block: report.l2_safe_block,
            l2_finalized_block: report.l2_finalized_block,
            respected_game_type: report.respected_game_type,
            system_paused: report.system_paused,
            total_games: report.total_games,
            anchor_l2_block: report.anchor_l2_block,
            anchor_root: report.anchor_root,
            proposals: report.proposals.iter().map(ProofsProposalJson::from_proposal).collect(),
            gaps: report.gaps,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsProposalJson {
    factory_index: u64,
    game_type: u32,
    game_address: Address,
    l2_block: Option<u64>,
    root_claim: Option<B256>,
    status: Option<u8>,
    created_at: TimestampJson,
}

impl ProofsProposalJson {
    fn from_proposal(proposal: &ProofsProposal) -> Self {
        Self {
            factory_index: proposal.factory_index,
            game_type: proposal.game_type,
            game_address: proposal.game_address,
            l2_block: proposal.l2_block,
            root_claim: proposal.root_claim,
            status: proposal.status,
            created_at: TimestampJson::from_unix(proposal.created_at),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProverProofsJson {
    total_count: u64,
    offset: u64,
    limit: u32,
    jobs: Vec<ProverProofSummaryJson>,
}

impl ProverProofsJson {
    fn try_from_page(page: &ProverProofsPage) -> Result<Self> {
        Ok(Self {
            total_count: page.total_count,
            offset: page.offset,
            limit: page.limit,
            jobs: page
                .jobs
                .iter()
                .map(ProverProofSummaryJson::try_from_job)
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProverProofSummaryJson {
    session_id: String,
    proof_type: String,
    status: ProofsJobStatus,
    created_at: TimestampJson,
    updated_at: TimestampJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<TimestampJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tee_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zk_vm: Option<String>,
}

impl ProverProofSummaryJson {
    fn try_from_job(job: &ProverProofSummary) -> Result<Self> {
        Ok(Self {
            session_id: job.session_id.clone(),
            proof_type: job.proof_type.clone(),
            status: job.status,
            created_at: TimestampJson::try_from_datetime_utc(job.created_at).with_context(
                || format!("converting created_at for proof job {}", job.session_id),
            )?,
            updated_at: TimestampJson::try_from_datetime_utc(job.updated_at).with_context(
                || format!("converting updated_at for proof job {}", job.session_id),
            )?,
            completed_at: job
                .completed_at
                .map(TimestampJson::try_from_datetime_utc)
                .transpose()
                .with_context(|| {
                    format!("converting completed_at for proof job {}", job.session_id)
                })?,
            error_message: job.error_message.clone(),
            tee_kind: job.tee_kind.clone(),
            zk_vm: job.zk_vm.clone(),
        })
    }
}

fn print_pretty(report: &ProofsListReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    write_pretty(&mut stdout, report)?;
    Ok(())
}

fn write_pretty<W: Write>(writer: &mut W, report: &ProofsListReport) -> io::Result<()> {
    writeln!(writer, "network  {}", report.request.network)?;
    writeln!(writer)?;
    write_onchain_section(writer, report)?;
    writeln!(writer)?;
    write_prover_section(writer, report)
}

fn write_onchain_section<W: Write>(writer: &mut W, report: &ProofsListReport) -> io::Result<()> {
    match &report.onchain {
        SourceResult::Available(onchain) => {
            writeln!(writer, "on-chain  ok")?;
            if let Some(source) = &report.request.onchain {
                writeln!(writer, "  rpc        l1={}  l2={}", source.l1_rpc, source.l2_rpc,)?;
                writeln!(
                    writer,
                    "  contracts  factory={}  anchor={}",
                    short_hex(source.contracts.dispute_game_factory),
                    short_hex(source.contracts.anchor_state_registry),
                )?;
            }
            writeln!(writer)?;
            writeln!(
                writer,
                "  heads      l1={}  latest={}  safe={}  finalized={}",
                fmt_opt(onchain.l1_block),
                fmt_opt(onchain.l2_latest_block),
                fmt_opt(onchain.l2_safe_block),
                fmt_opt(onchain.l2_finalized_block),
            )?;
            writeln!(
                writer,
                "  anchor     l2={}  root={}  paused={}",
                fmt_opt(onchain.anchor_l2_block),
                onchain.anchor_root.map_or_else(|| "unknown".to_string(), short_hex),
                fmt_opt_bool(onchain.system_paused),
            )?;
            writeln!(
                writer,
                "  games      total={}  respected_type={}",
                fmt_opt(onchain.total_games),
                onchain
                    .respected_game_type
                    .map_or_else(|| "unknown".to_string(), |game_type| game_type.to_string()),
            )?;
            writeln!(writer)?;
            write_proposals(writer, &onchain.proposals, report.request.scan_window)?;
            write_gaps(writer, onchain.gaps)?;
        }
        SourceResult::Skipped(reason) => {
            writeln!(writer, "on-chain  skipped ({reason})")?;
        }
        SourceResult::Error(error) => {
            writeln!(writer, "on-chain  error ({error})")?;
        }
    }
    Ok(())
}

fn write_gaps<W: Write>(writer: &mut W, gaps: ProofsGapReport) -> io::Result<()> {
    let rows = [
        ("proposer->safe", gaps.proposer_behind_safe_head),
        ("proposer->latest", gaps.proposer_behind_latest_head),
        ("anchor->proposal", gaps.anchor_behind_latest_proposal),
        ("anchor->safe", gaps.anchor_behind_safe_head),
    ];
    let rendered = rows
        .into_iter()
        .filter_map(|(label, blocks)| blocks.map(|blocks| format!("{label}: {blocks} blocks")))
        .collect::<Vec<_>>();

    if rendered.is_empty() {
        writeln!(writer, "  gaps       unavailable")
    } else {
        writeln!(writer, "  gaps       {}", rendered.join("  "))
    }
}

fn write_proposals<W: Write>(
    writer: &mut W,
    proposals: &[ProofsProposal],
    scan_window: u64,
) -> io::Result<()> {
    if proposals.is_empty() {
        writeln!(writer, "  proposals  none found in last {scan_window} games")?;
        return Ok(());
    }

    writeln!(writer, "  proposals")?;
    for proposal in proposals {
        writeln!(
            writer,
            "    #{} type={} l2={} status={} created={} address={} root={}",
            proposal.factory_index,
            proposal.game_type,
            fmt_opt(proposal.l2_block),
            proposal_status_label(proposal.status),
            format_unix_timestamp(proposal.created_at),
            short_hex(proposal.game_address),
            fmt_opt_hex(proposal.root_claim),
        )?;
    }
    Ok(())
}

fn write_prover_section<W: Write>(writer: &mut W, report: &ProofsListReport) -> io::Result<()> {
    match &report.prover {
        SourceResult::Available(page) => {
            writeln!(writer, "prover jobs  ok")?;
            if let Some(url) = &report.request.prover_url {
                writeln!(writer, "  url        {url}")?;
            }
            if let Some(status) = report.request.status {
                writeln!(writer, "  filter     status={status}")?;
            }
            writeln!(
                writer,
                "  page       total={}  offset={}  limit={}",
                page.total_count, page.offset, page.limit,
            )?;
            if page.jobs.is_empty() {
                writeln!(writer, "  jobs       none")?;
            } else {
                writeln!(writer, "  jobs")?;
                for job in &page.jobs {
                    writeln!(
                        writer,
                        "    {}  type={}  status={}  created={}  updated={}",
                        job.session_id,
                        job.proof_type,
                        job.status,
                        job.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
                        job.updated_at.format("%Y-%m-%d %H:%M:%S UTC"),
                    )?;
                    if let Some(error) = &job.error_message {
                        writeln!(writer, "      error: {error}")?;
                    }
                }
            }
        }
        SourceResult::Skipped(reason) => {
            writeln!(writer, "prover jobs  skipped ({reason})")?;
        }
        SourceResult::Error(error) => {
            writeln!(writer, "prover jobs  error ({error})")?;
        }
    }
    Ok(())
}

fn fmt_opt(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
}

const fn fmt_opt_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn short_hex(value: impl fmt::LowerHex) -> String {
    shorten_prefixed_hex(&format!("{value:#x}"))
}

fn fmt_opt_hex(value: Option<impl fmt::LowerHex>) -> String {
    value.map_or_else(|| "unknown".to_string(), short_hex)
}

fn shorten_prefixed_hex(value: &str) -> String {
    const PREFIX_LEN: usize = 6;
    const SUFFIX_LEN: usize = 4;
    const MIN_OMITTED: usize = 4;

    if value.len() <= PREFIX_LEN + SUFFIX_LEN + MIN_OMITTED {
        return value.to_string();
    }

    format!("{}...{}", &value[..PREFIX_LEN], &value[value.len() - SUFFIX_LEN..])
}

const fn proposal_status_label(status: Option<u8>) -> &'static str {
    match status {
        Some(0) => "in_progress",
        Some(1) => "challenger_wins",
        Some(2) => "defender_wins",
        Some(_) | None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};
    use anyhow::{Context, anyhow};
    use basectl_cli::{
        MonitoringConfig, OnchainProofsReport, ProofsCommandError, ProofsConfig, ProofsGapReport,
        ProofsJobStatus, ProofsProposal, ProverProofSummary, ProverProofsPage,
    };
    use chrono::{TimeZone, Utc};
    use serde_json::Value;
    use url::Url;

    use super::{ProofsListJson, ProofsListReport, ProofsListRequest, SourceResult, write_pretty};
    use crate::cli::ProofsListArgs;

    fn test_config(proofs: Option<ProofsConfig>) -> MonitoringConfig {
        MonitoringConfig {
            name: "devnet".to_string(),
            rpc: Url::parse("http://127.0.0.1:7545").unwrap(),
            flashblocks_ws: Url::parse("ws://127.0.0.1:7111").unwrap(),
            l1_rpc: Url::parse("http://127.0.0.1:4545").unwrap(),
            consensus_node_rpc: None,
            upgrades: None,
            system_config: Address::ZERO,
            batcher_address: None,
            l1_blob_target: 14,
            conductors: None,
            discovery: None,
            validators: None,
            proofs,
            pods: None,
        }
    }

    fn test_args(mut apply: impl FnMut(&mut ProofsListArgs)) -> ProofsListArgs {
        let mut args = ProofsListArgs {
            l1_rpc: None,
            l2_rpc: None,
            dispute_game_factory: None,
            anchor_state_registry: None,
            prover_url: None,
            status: None,
            limit: 10,
            offset: 0,
            scan_window: 50,
            json: false,
        };
        apply(&mut args);
        args
    }

    fn test_proofs_config() -> ProofsConfig {
        ProofsConfig {
            dispute_game_factory: Address::repeat_byte(0x11),
            anchor_state_registry: Address::repeat_byte(0x22),
            prover_url: None,
        }
    }

    #[test]
    fn status_without_prover_source_returns_typed_error() {
        let args = test_args(|args| args.status = Some(ProofsJobStatus::Running));

        let err =
            ProofsListRequest::resolve(test_config(Some(test_proofs_config())), args).unwrap_err();

        assert!(matches!(err, ProofsCommandError::MissingProverSource { flag: "--status" }));
    }

    #[test]
    fn offset_without_prover_source_returns_typed_error() {
        let args = test_args(|args| args.offset = 10);

        let err =
            ProofsListRequest::resolve(test_config(Some(test_proofs_config())), args).unwrap_err();

        assert!(matches!(err, ProofsCommandError::MissingProverSource { flag: "--offset" }));
    }

    #[test]
    fn partial_contract_override_returns_typed_error() {
        let args = test_args(|args| args.dispute_game_factory = Some(Address::repeat_byte(0x33)));

        let err = ProofsListRequest::resolve(test_config(None), args).unwrap_err();

        assert!(matches!(err, ProofsCommandError::PartialContractOverride));
    }

    #[test]
    fn invalid_limit_and_scan_window_return_typed_errors() {
        let err = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| {
                args.limit = 0;
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ProofsCommandError::LimitOutOfRange { limit: 0 }));

        let err = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| {
                args.limit = 101;
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ProofsCommandError::LimitOutOfRange { limit: 101 }));

        let err = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| {
                args.scan_window = 1001;
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ProofsCommandError::ScanWindowOutOfRange { scan_window: 1001 }));
    }

    #[test]
    fn missing_all_sources_returns_typed_error() {
        let err = ProofsListRequest::resolve(test_config(None), test_args(|_| {})).unwrap_err();

        assert!(
            matches!(err, ProofsCommandError::MissingSource { config_name } if config_name == "devnet")
        );
    }

    #[test]
    fn json_report_shape_includes_top_level_sections() {
        let request = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| args.json = true),
        )
        .unwrap();
        let report = ProofsListReport {
            request,
            onchain: SourceResult::Available(sample_onchain_report()),
            prover: SourceResult::Skipped("no prover-service URL configured".to_string()),
        };

        let value: Value = serde_json::to_value(ProofsListJson::from_report(&report)).unwrap();

        assert_eq!(value["network"], "devnet");
        assert!(value.get("inputs").is_some());
        assert!(value.get("onchain").is_some());
        assert!(value.get("prover").is_some());
        assert_eq!(value["onchain"]["status"], "available");
        assert_eq!(value["prover"]["status"], "skipped");
    }

    #[test]
    fn json_report_shape_includes_valid_prover_jobs() {
        let request = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| {
                args.json = true;
                args.prover_url = Some(Url::parse("http://127.0.0.1:7300").unwrap());
            }),
        )
        .unwrap();
        let report = ProofsListReport {
            request,
            onchain: SourceResult::Available(sample_onchain_report()),
            prover: SourceResult::Available(sample_prover_page(valid_timestamp())),
        };

        let json = ProofsListJson::from_report(&report);
        let value: Value = serde_json::to_value(json).unwrap();

        assert_eq!(value["prover"]["status"], "available");
        assert_eq!(value["prover"]["data"]["jobs"][0]["createdAt"]["unix"], 1_780_596_004);
    }

    #[test]
    fn json_report_maps_pre_epoch_prover_timestamp_to_source_error() {
        let request = ProofsListRequest::resolve(
            test_config(Some(test_proofs_config())),
            test_args(|args| {
                args.json = true;
                args.prover_url = Some(Url::parse("http://127.0.0.1:7300").unwrap());
            }),
        )
        .unwrap();
        let report = ProofsListReport {
            request,
            onchain: SourceResult::Skipped("no proof contract addresses configured".to_string()),
            prover: SourceResult::Available(sample_prover_page(
                Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).single().unwrap(),
            )),
        };

        let json = ProofsListJson::from_report(&report);
        let value: Value = serde_json::to_value(&json).unwrap();

        assert!(json.has_error());
        assert_eq!(value["prover"]["status"], "error");
        assert!(value["prover"].get("data").is_none());
        assert!(
            value["prover"]["error"]
                .as_str()
                .unwrap()
                .contains("converting created_at for proof job session-1"),
        );
        assert!(value["prover"]["error"].as_str().unwrap().contains("before the Unix epoch"),);
    }

    #[test]
    fn source_result_preserves_anyhow_error_chain() {
        let result = Err::<(), _>(anyhow!("transport refused"))
            .context("connecting to L1 RPC at http://127.0.0.1:1");

        let SourceResult::Error(error) = SourceResult::from_result(result) else {
            panic!("expected error result");
        };

        assert!(error.contains("connecting to L1 RPC at http://127.0.0.1:1"));
        assert!(error.contains("transport refused"));
    }

    #[test]
    fn pretty_rendering_covers_available_skipped_and_error_sections() {
        let request =
            ProofsListRequest::resolve(test_config(Some(test_proofs_config())), test_args(|_| {}))
                .unwrap();
        let report = ProofsListReport {
            request,
            onchain: SourceResult::Available(sample_onchain_report()),
            prover: SourceResult::Skipped("no prover-service URL configured".to_string()),
        };
        let mut out = Vec::new();
        write_pretty(&mut out, &report).unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("on-chain  ok"));
        assert!(rendered.contains("contracts  factory=0x1111...1111  anchor=0x2222...2222"));
        assert!(rendered.contains("proposals"));
        assert!(rendered.contains("status=in_progress"));
        assert!(rendered.contains("root=0xbbbb...bbbb"));
        assert!(rendered.contains("prover jobs"));
        assert!(rendered.contains("prover jobs  skipped (no prover-service URL configured)"));

        let mut error_report = report.clone();
        error_report.onchain = SourceResult::Error("rpc failed".to_string());
        let mut out = Vec::new();
        write_pretty(&mut out, &error_report).unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("on-chain  error (rpc failed)"));
        assert!(rendered.contains("rpc failed"));
    }

    fn sample_onchain_report() -> OnchainProofsReport {
        OnchainProofsReport {
            l1_block: Some(100),
            l2_latest_block: Some(1_000),
            l2_safe_block: Some(900),
            l2_finalized_block: Some(800),
            respected_game_type: Some(0),
            system_paused: Some(false),
            total_games: Some(2),
            anchor_l2_block: Some(850),
            anchor_root: Some(B256::repeat_byte(0xaa)),
            proposals: vec![ProofsProposal {
                factory_index: 1,
                game_type: 0,
                game_address: Address::repeat_byte(0x44),
                l2_block: Some(875),
                root_claim: Some(B256::repeat_byte(0xbb)),
                status: Some(0),
                created_at: 1_780_270_000,
            }],
            gaps: ProofsGapReport {
                proposer_behind_safe_head: Some(25),
                proposer_behind_latest_head: Some(125),
                anchor_behind_latest_proposal: Some(25),
                anchor_behind_safe_head: Some(50),
            },
        }
    }

    fn sample_prover_page(timestamp: chrono::DateTime<Utc>) -> ProverProofsPage {
        ProverProofsPage {
            jobs: vec![ProverProofSummary {
                session_id: "session-1".to_string(),
                proof_type: "compressed".to_string(),
                status: ProofsJobStatus::Succeeded,
                created_at: timestamp,
                updated_at: valid_timestamp(),
                completed_at: Some(valid_timestamp()),
                error_message: None,
                tee_kind: None,
                zk_vm: Some("sp1".to_string()),
            }],
            total_count: 1,
            offset: 0,
            limit: 10,
        }
    }

    fn valid_timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 4, 18, 0, 4).single().unwrap()
    }
}
