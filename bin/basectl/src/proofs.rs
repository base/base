//! Implementation of the `basectl proofs` command group.

use std::io::{self, Write};

use anyhow::Result;
use base_prover_service_protocol::{
    GetProofResponse, ListProofsRequest, ProofResult, ProofStatus, ProofSummary, ProofType,
    TeeKind, ZkVm,
};
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, ProofFinalizeRequest, ProofsClient,
    ProofsCommandError,
};
use serde::Serialize;
use tracing::info;
use url::Url;

use crate::{
    cli::{
        ProofStatusFilter, ProofsCommands, ProofsFinalizeArgs, ProofsListArgs, ProofsStatusArgs,
    },
    confirm::confirm_or_abort,
    helpers::CommandOutcome,
};

/// Runs the `basectl proofs` command group.
pub(crate) async fn run(
    config: MonitoringConfig,
    command: ProofsCommands,
) -> Result<CommandOutcome> {
    match command {
        ProofsCommands::Finalize(args) => run_finalize(config, args).await,
        ProofsCommands::Status(args) => run_status(config, args).await,
        ProofsCommands::List(args) => run_list(config, args).await,
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
    if !confirm_or_abort(&prompt, yes)? {
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
        status = ProofsClient::status_label(response.status),
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

async fn run_list(config: MonitoringConfig, args: ProofsListArgs) -> Result<CommandOutcome> {
    let ProofsListArgs { status, offset, limit, prover_rpc, json } = args;
    let endpoint = resolve_prover_rpc(&config, prover_rpc)?;
    let status_filter = status.map(proof_status_from_filter);
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

/// Maps the CLI status filter to the prover-service protocol status.
const fn proof_status_from_filter(filter: ProofStatusFilter) -> ProofStatus {
    match filter {
        ProofStatusFilter::Queued => ProofStatus::Queued,
        ProofStatusFilter::Running => ProofStatus::Running,
        ProofStatusFilter::Succeeded => ProofStatus::Succeeded,
        ProofStatusFilter::Failed => ProofStatus::Failed,
    }
}

/// Returns the CLI label for a proof type.
const fn proof_type_label(proof_type: ProofType) -> &'static str {
    match proof_type {
        ProofType::Compressed => "compressed",
        ProofType::SnarkPlonk => "snark_plonk",
        ProofType::Tee => "tee",
    }
}

/// Returns the CLI label for a ZK virtual machine.
const fn zk_vm_label(zk_vm: ZkVm) -> &'static str {
    match zk_vm {
        ZkVm::Sp1 => "sp1",
    }
}

/// Returns the CLI label for a TEE implementation.
const fn tee_kind_label(tee_kind: TeeKind) -> &'static str {
    match tee_kind {
        TeeKind::AwsNitro => "aws_nitro",
    }
}

/// Humanized JSON shape for a `basectl proofs finalize` outcome.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsFinalizeJson {
    network: String,
    prover_rpc: String,
    session_id: String,
    start_block: u64,
    end_block: u64,
    num_blocks: u64,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ProofResultJson>,
}

impl ProofsFinalizeJson {
    fn submitted(
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
            status: "submitted",
            error_message: None,
            result: None,
        }
    }

    fn completed(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        start_block: u64,
        num_blocks: u64,
        response: &GetProofResponse,
    ) -> Self {
        Self {
            status: ProofsClient::status_label(response.status),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
            ..Self::submitted(network, prover_rpc, session_id, start_block, num_blocks)
        }
    }
}

/// Humanized JSON shape for `basectl proofs status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsStatusJson {
    network: String,
    prover_rpc: String,
    session_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ProofResultJson>,
}

impl ProofsStatusJson {
    fn from_response(
        network: &str,
        prover_rpc: &Url,
        session_id: &str,
        response: &GetProofResponse,
    ) -> Self {
        Self {
            network: network.to_string(),
            prover_rpc: prover_rpc.to_string(),
            session_id: session_id.to_string(),
            status: ProofsClient::status_label(response.status),
            error_message: response.error_message.clone(),
            result: response.result.as_ref().map(ProofResultJson::from_result),
        }
    }
}

/// Humanized summary of a proof result payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofResultJson {
    proof_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    zk_vm: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tee_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_bytes: Option<usize>,
}

impl ProofResultJson {
    fn from_result(result: &ProofResult) -> Self {
        match result {
            ProofResult::Compressed(zk) => Self {
                proof_type: "compressed",
                zk_vm: Some(zk_vm_label(zk.zk_vm)),
                tee_kind: None,
                proof_bytes: Some(zk.proof.len()),
            },
            ProofResult::SnarkPlonk(plonk) => Self {
                proof_type: "snark_plonk",
                zk_vm: Some(zk_vm_label(plonk.proof.zk_vm)),
                tee_kind: None,
                proof_bytes: Some(plonk.proof.proof.len()),
            },
            ProofResult::Tee(tee) => Self {
                proof_type: "tee",
                zk_vm: None,
                tee_kind: Some(tee_kind_label(tee.tee_kind)),
                proof_bytes: None,
            },
        }
    }
}

/// Humanized JSON shape for `basectl proofs list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofsListJson {
    network: String,
    prover_rpc: String,
    offset: u64,
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_filter: Option<&'static str>,
    total_count: u64,
    proofs: Vec<ProofSummaryJson>,
}

impl ProofsListJson {
    fn from_response(
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
            status_filter: status_filter.map(ProofsClient::status_label),
            total_count,
            proofs: proofs.iter().map(ProofSummaryJson::from_summary).collect(),
        }
    }
}

/// Humanized JSON row for one submitted proof request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofSummaryJson {
    session_id: String,
    proof_type: &'static str,
    status: &'static str,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tee_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zk_vm: Option<&'static str>,
}

impl ProofSummaryJson {
    fn from_summary(summary: &ProofSummary) -> Self {
        Self {
            session_id: summary.session_id.clone(),
            proof_type: proof_type_label(summary.proof_type),
            status: ProofsClient::status_label(summary.status),
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            completed_at: summary.completed_at.map(|at| at.to_rfc3339()),
            error_message: summary.error_message.clone(),
            tee_kind: summary.tee_kind.map(tee_kind_label),
            zk_vm: summary.zk_vm.map(zk_vm_label),
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
        .row("status", outcome.status);
    if let Some(error_message) = &outcome.error_message {
        table.row("error", error_message);
    }
    if let Some(result) = &outcome.result {
        append_result_rows(&mut table, result);
    }
    table.render(writer)?;
    if outcome.status == "submitted" {
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
        .row("status", status.status);
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
        table.row("status filter", status_filter);
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
    use base_prover_service_protocol::{
        GetProofResponse, ProofResult, ProofStatus, ProofSummary, ProofType, ZkProofResult, ZkVm,
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
        use base_prover_service_protocol::SnarkPlonkProofResult;

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
