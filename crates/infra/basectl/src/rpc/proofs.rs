use std::{fmt, str::FromStr, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::sol;
use alloy_transport_http::Http;
use anyhow::{Context, Result, ensure};
use base_common_network::Base;
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    ListProofsRequest, ProofStatus, ProofSummary, ProofType, TeeKind, ZkVm,
};
use chrono::{DateTime, Utc};
use futures::future;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;
use url::Url;

use crate::{config::ProofsConfig, tui::Toast};

const GAME_SCAN_BATCH_SIZE: usize = 50;

sol! {
    #[sol(rpc)]
    interface IAnchorStateRegistry {
        function getAnchorRoot() external view returns (bytes32 root, uint256 l2SequenceNumber);
        function respectedGameType() external view returns (uint32);
        function paused() external view returns (bool);
    }

    #[sol(rpc)]
    interface IDisputeGameFactory {
        function gameCount() external view returns (uint256);
        function gameAtIndex(uint256 index) external view returns (
            uint32 gameType, uint64 timestamp, address proxy
        );
    }

    #[sol(rpc)]
    interface IAggregateVerifier {
        function rootClaim() external pure returns (bytes32);
        function l2SequenceNumber() external pure returns (uint256);
        function status() external view returns (uint8);
    }
}

/// Client helpers for proof-system and prover-service reads.
#[derive(Debug, Clone, Copy)]
pub struct ProofsClient;

impl ProofsClient {
    /// Maximum number of on-chain proof proposals returned by one report request.
    pub const MAX_ONCHAIN_REPORT_LIMIT: u32 = 100;
    /// Maximum number of dispute games scanned by one on-chain report request.
    pub const MAX_ONCHAIN_SCAN_WINDOW: u64 = 1_000;

    /// Fetches a combined on-chain proof-system report from L1/L2 RPCs.
    #[must_use = "callers should handle the fetched on-chain proof report"]
    pub async fn fetch_onchain_report(
        proofs_config: &ProofsConfig,
        l1_rpc: &Url,
        l2_rpc: &Url,
        scan_window: u64,
        limit: u32,
    ) -> Result<OnchainProofsReport> {
        ensure!(
            (1..=Self::MAX_ONCHAIN_SCAN_WINDOW).contains(&scan_window),
            "proof on-chain report scan window must be between 1 and {}",
            Self::MAX_ONCHAIN_SCAN_WINDOW
        );
        ensure!(
            (1..=Self::MAX_ONCHAIN_REPORT_LIMIT).contains(&limit),
            "proof on-chain report limit must be between 1 and {}",
            Self::MAX_ONCHAIN_REPORT_LIMIT
        );

        let l1_provider = Arc::new(
            ProviderBuilder::new()
                .connect(l1_rpc.as_str())
                .await
                .with_context(|| format!("connecting to L1 RPC at {l1_rpc}"))?,
        );

        let http_client = alloy_transport_http::reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .with_context(|| format!("building L2 HTTP client for {l2_rpc}"))?;
        let transport = Http::with_client(http_client, l2_rpc.clone());
        let l2_provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_client(RpcClient::new(transport, false));

        let asr = IAnchorStateRegistry::new(proofs_config.anchor_state_registry, &*l1_provider);
        let factory = IDisputeGameFactory::new(proofs_config.dispute_game_factory, &*l1_provider);

        fetch_onchain_report_from_providers(
            &asr,
            &factory,
            &*l1_provider,
            &l2_provider,
            scan_window,
            limit,
        )
        .await
    }

    /// Lists submitted prover-service proof jobs.
    #[must_use = "callers should handle the fetched prover job page"]
    pub async fn list_prover_jobs(
        prover_url: &Url,
        request: ProofsJobListRequest,
    ) -> Result<ProverProofsPage> {
        let config = ProverServiceClientConfig::new(prover_url.to_string())
            .with_request_timeout(Duration::from_secs(10));
        let client = ProofRequesterClient::connect(&config)
            .with_context(|| format!("connecting to prover service at {prover_url}"))?;
        let response = client
            .list_proofs(request.into_protocol())
            .await
            .with_context(|| format!("listing proofs from prover service at {prover_url}"))?;

        Ok(ProverProofsPage::from_response(response, request.offset, request.limit))
    }
}

/// Snapshot of proof system state, fetched periodically.
#[derive(Debug, Clone)]
pub struct ProofsSnapshot {
    /// Current L1 block number.
    pub l1_block: Option<u64>,
    /// Current L2 latest (unsafe) block number.
    pub l2_latest_block: Option<u64>,
    /// Current L2 safe block number.
    pub l2_safe_block: Option<u64>,
    /// Current L2 finalized block number.
    pub l2_finalized_block: Option<u64>,
    /// Respected game type from the `AnchorStateRegistry`.
    pub respected_game_type: Option<u32>,
    /// Whether the proof system is paused.
    pub system_paused: Option<bool>,
    /// Total number of dispute games created.
    pub total_games: Option<u64>,
    /// Anchor L2 block number (latest finalized anchor).
    pub anchor_l2_block: Option<u64>,
    /// Anchor output root hash.
    pub anchor_root: Option<B256>,
    /// Most recent dispute game proposal.
    pub latest_proposal: Option<LatestProposal>,
}

impl From<OnchainProofsReport> for ProofsSnapshot {
    fn from(report: OnchainProofsReport) -> Self {
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
            latest_proposal: report.proposals.first().cloned().map(LatestProposal::from),
        }
    }
}

/// Information about the most recent dispute game proposal.
#[derive(Debug, Clone)]
pub struct LatestProposal {
    /// Address of the dispute game proxy contract.
    pub game_address: Address,
    /// L2 block number proposed.
    pub l2_block: Option<u64>,
    /// Output root claimed by the proposal.
    pub root_claim: Option<B256>,
    /// Game status: `0`=`IN_PROGRESS`, `1`=`CHALLENGER_WINS`, `2`=`DEFENDER_WINS`.
    pub status: Option<u8>,
    /// L1 timestamp when the game was created.
    pub created_at: u64,
}

impl From<ProofsProposal> for LatestProposal {
    fn from(proposal: ProofsProposal) -> Self {
        Self {
            game_address: proposal.game_address,
            l2_block: proposal.l2_block,
            root_claim: proposal.root_claim,
            status: proposal.status,
            created_at: proposal.created_at,
        }
    }
}

/// Combined on-chain proof-system state for `basectl proofs list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnchainProofsReport {
    /// Current L1 block number.
    pub l1_block: Option<u64>,
    /// Current L2 latest (unsafe) block number.
    pub l2_latest_block: Option<u64>,
    /// Current L2 safe block number.
    pub l2_safe_block: Option<u64>,
    /// Current L2 finalized block number.
    pub l2_finalized_block: Option<u64>,
    /// Respected game type from the `AnchorStateRegistry`.
    pub respected_game_type: Option<u32>,
    /// Whether the proof system is paused.
    pub system_paused: Option<bool>,
    /// Total number of dispute games created.
    pub total_games: Option<u64>,
    /// Anchor L2 block number.
    pub anchor_l2_block: Option<u64>,
    /// Anchor output root hash.
    pub anchor_root: Option<B256>,
    /// Recent respected-game proposals, newest first by factory index.
    pub proposals: Vec<ProofsProposal>,
    /// Derived sync/proposal gap metrics.
    pub gaps: ProofsGapReport,
}

/// Recent dispute-game proposal details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsProposal {
    /// Factory array index for this dispute game.
    pub factory_index: u64,
    /// Dispute game type.
    pub game_type: u32,
    /// Address of the dispute game proxy contract.
    pub game_address: Address,
    /// L2 block number proposed.
    pub l2_block: Option<u64>,
    /// Output root claimed by the proposal.
    pub root_claim: Option<B256>,
    /// Game status: `0`=`IN_PROGRESS`, `1`=`CHALLENGER_WINS`, `2`=`DEFENDER_WINS`.
    pub status: Option<u8>,
    /// L1 timestamp when the game was created.
    pub created_at: u64,
}

/// Derived proof-system gap metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsGapReport {
    /// Safe head minus latest proposal, saturating at zero.
    pub proposer_behind_safe_head: Option<u64>,
    /// Latest head minus latest proposal, saturating at zero.
    pub proposer_behind_latest_head: Option<u64>,
    /// Latest proposal minus anchor, saturating at zero.
    pub anchor_behind_latest_proposal: Option<u64>,
    /// Safe head minus anchor, saturating at zero.
    pub anchor_behind_safe_head: Option<u64>,
}

impl ProofsGapReport {
    /// Computes gap metrics from optional heads, anchor, and proposal height.
    pub fn from_heads(
        l2_latest_block: Option<u64>,
        l2_safe_block: Option<u64>,
        anchor_l2_block: Option<u64>,
        latest_proposal_l2_block: Option<u64>,
    ) -> Self {
        Self {
            proposer_behind_safe_head: l2_safe_block
                .zip(latest_proposal_l2_block)
                .map(|(safe, proposed)| safe.saturating_sub(proposed)),
            proposer_behind_latest_head: l2_latest_block
                .zip(latest_proposal_l2_block)
                .map(|(latest, proposed)| latest.saturating_sub(proposed)),
            anchor_behind_latest_proposal: anchor_l2_block
                .zip(latest_proposal_l2_block)
                .map(|(anchor, proposed)| proposed.saturating_sub(anchor)),
            anchor_behind_safe_head: anchor_l2_block
                .zip(l2_safe_block)
                .map(|(anchor, safe)| safe.saturating_sub(anchor)),
        }
    }
}

/// Paginated prover-service job list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProverProofsPage {
    /// Submitted proof jobs in service order.
    pub jobs: Vec<ProverProofSummary>,
    /// Total matching proof count.
    pub total_count: u64,
    /// Number of rows skipped.
    pub offset: u64,
    /// Maximum rows requested.
    pub limit: u32,
}

impl ProverProofsPage {
    /// Converts a prover-service protocol response into basectl's normalized shape.
    pub fn from_response(
        response: base_prover_service_protocol::ListProofsResponse,
        offset: u64,
        limit: u32,
    ) -> Self {
        Self {
            jobs: response.proofs.into_iter().map(ProverProofSummary::from).collect(),
            total_count: response.total_count,
            offset,
            limit,
        }
    }
}

/// Normalized summary of a submitted prover-service proof request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProverProofSummary {
    /// Proof session identifier.
    pub session_id: String,
    /// Proof type as a stable `snake_case` label.
    pub proof_type: String,
    /// Current proof status.
    pub status: ProofsJobStatus,
    /// Timestamp when the proof request was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the proof request was last updated.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the proof request completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Error message when the proof failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// TEE implementation for TEE proofs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_kind: Option<String>,
    /// ZK virtual machine for ZK proofs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_vm: Option<String>,
}

impl From<ProofSummary> for ProverProofSummary {
    fn from(summary: ProofSummary) -> Self {
        Self {
            session_id: summary.session_id,
            proof_type: proof_type_label(summary.proof_type).to_string(),
            status: ProofsJobStatus::from(summary.status),
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            completed_at: summary.completed_at,
            error_message: summary.error_message,
            tee_kind: summary.tee_kind.map(tee_kind_label).map(str::to_string),
            zk_vm: summary.zk_vm.map(zk_vm_label).map(str::to_string),
        }
    }
}

/// Status filter for submitted prover-service proof jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofsJobStatus {
    /// Proof request is queued.
    Queued,
    /// Proof request is running.
    Running,
    /// Proof request completed successfully.
    Succeeded,
    /// Proof request failed.
    Failed,
}

impl ProofsJobStatus {
    /// Returns the stable `snake_case` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    /// Converts this basectl status into the prover-service protocol status.
    pub const fn into_protocol(self) -> ProofStatus {
        match self {
            Self::Queued => ProofStatus::Queued,
            Self::Running => ProofStatus::Running,
            Self::Succeeded => ProofStatus::Succeeded,
            Self::Failed => ProofStatus::Failed,
        }
    }
}

impl From<ProofStatus> for ProofsJobStatus {
    fn from(status: ProofStatus) -> Self {
        match status {
            ProofStatus::Queued => Self::Queued,
            ProofStatus::Running => Self::Running,
            ProofStatus::Succeeded => Self::Succeeded,
            ProofStatus::Failed => Self::Failed,
        }
    }
}

impl FromStr for ProofsJobStatus {
    type Err = ProofsJobStatusParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            _ => Err(ProofsJobStatusParseError { value: value.to_string() }),
        }
    }
}

impl fmt::Display for ProofsJobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing a prover proof status filter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid proof status `{value}`; expected queued, running, succeeded, or failed")]
pub struct ProofsJobStatusParseError {
    /// Raw status value supplied by the caller.
    pub value: String,
}

/// Request parameters for listing prover-service proof jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofsJobListRequest {
    /// Number of rows to skip.
    pub offset: u64,
    /// Maximum rows to return.
    pub limit: u32,
    /// Optional status filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ProofsJobStatus>,
}

impl ProofsJobListRequest {
    /// Converts the normalized request into the prover-service protocol request.
    pub fn into_protocol(self) -> ListProofsRequest {
        ListProofsRequest {
            offset: self.offset,
            limit: self.limit,
            status_filter: self.status.map(ProofsJobStatus::into_protocol),
        }
    }
}

/// Polls proof system state at regular intervals and sends snapshots to the TUI.
pub async fn run_proofs_poller(
    proofs_config: ProofsConfig,
    l1_rpc: Url,
    l2_rpc: Url,
    tx: mpsc::Sender<ProofsSnapshot>,
    toast_tx: mpsc::Sender<Toast>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        interval.tick().await;

        let report =
            ProofsClient::fetch_onchain_report(&proofs_config, &l1_rpc, &l2_rpc, 50, 1).await;
        let snapshot = match report {
            Ok(report) => ProofsSnapshot::from(report),
            Err(error) => {
                warn!(error = %error, "Failed to fetch proof system state");
                let _ = toast_tx.try_send(Toast::warning("Proofs: state fetch failed"));
                continue;
            }
        };

        if tx.send(snapshot).await.is_err() {
            break;
        }
    }
}

async fn fetch_onchain_report_from_providers<P: Provider + Clone>(
    asr: &IAnchorStateRegistry::IAnchorStateRegistryInstance<&P>,
    factory: &IDisputeGameFactory::IDisputeGameFactoryInstance<&P>,
    l1_provider: &P,
    l2_provider: &impl Provider<Base>,
    scan_window: u64,
    limit: u32,
) -> Result<OnchainProofsReport> {
    let (chain, anchor, game_type, paused, game_count) = tokio::join!(
        fetch_chain_heads(l1_provider, l2_provider),
        async { asr.getAnchorRoot().call().await.ok() },
        async { asr.respectedGameType().call().await.ok() },
        async { asr.paused().call().await.ok() },
        async { factory.gameCount().call().await.ok() },
    );

    let (l1_block, l2_latest, l2_safe, l2_finalized) = chain;
    let total_games = game_count.and_then(|count| count.try_into().ok());
    let proposals =
        find_recent_proposals(factory, l1_provider, game_type, total_games, scan_window, limit)
            .await;

    let anchor_l2_block = anchor.as_ref().and_then(|a| a.l2SequenceNumber.try_into().ok());
    let anchor_root = anchor.map(|a| a.root);
    let gaps = ProofsGapReport::from_heads(
        l2_latest,
        l2_safe,
        anchor_l2_block,
        proposals.first().and_then(|proposal| proposal.l2_block),
    );

    Ok(OnchainProofsReport {
        l1_block,
        l2_latest_block: l2_latest,
        l2_safe_block: l2_safe,
        l2_finalized_block: l2_finalized,
        respected_game_type: game_type,
        system_paused: paused,
        total_games,
        anchor_l2_block,
        anchor_root,
        proposals,
        gaps,
    })
}

async fn fetch_chain_heads(
    l1: &impl Provider,
    l2: &impl Provider<Base>,
) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let (l1_block, l2_latest, l2_safe, l2_finalized) = tokio::join!(
        async { l1.get_block_number().await.ok() },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Latest)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Safe)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
        async {
            l2.get_block_by_number(BlockNumberOrTag::Finalized)
                .await
                .ok()
                .flatten()
                .map(|b| b.header.number)
        },
    );
    (l1_block, l2_latest, l2_safe, l2_finalized)
}

async fn find_recent_proposals<P: Provider + Clone>(
    factory: &IDisputeGameFactory::IDisputeGameFactoryInstance<&P>,
    l1_provider: &P,
    respected_type: Option<u32>,
    total_games: Option<u64>,
    scan_window: u64,
    limit: u32,
) -> Vec<ProofsProposal> {
    let Some(game_type) = respected_type else { return Vec::new() };
    let Some(count) = total_games.filter(|count| *count > 0) else { return Vec::new() };
    if limit == 0 {
        return Vec::new();
    }

    let scan_start = count - 1;
    let scan_end = count.saturating_sub(scan_window);
    let mut proposals = Vec::new();

    let indices = (scan_end..=scan_start).rev().collect::<Vec<_>>();
    for batch in indices.chunks(GAME_SCAN_BATCH_SIZE) {
        let games = future::join_all(batch.iter().copied().map(|idx| async move {
            factory.gameAtIndex(U256::from(idx)).call().await.ok().map(|game| (idx, game))
        }))
        .await;
        let remaining = limit as usize - proposals.len();
        let batch_proposals = future::join_all(
            games
                .into_iter()
                .flatten()
                .filter(|(_, game)| game.gameType == game_type)
                .take(remaining)
                .map(|(idx, game)| async move {
                    let verifier = IAggregateVerifier::new(game.proxy, l1_provider);
                    let (root_claim, l2_seq, status) = tokio::join!(
                        async { verifier.rootClaim().call().await.ok() },
                        async { verifier.l2SequenceNumber().call().await.ok() },
                        async { verifier.status().call().await.ok() },
                    );

                    ProofsProposal {
                        factory_index: idx,
                        game_type: game.gameType,
                        game_address: game.proxy,
                        l2_block: l2_seq.and_then(|seq| seq.try_into().ok()),
                        root_claim,
                        status,
                        created_at: game.timestamp,
                    }
                }),
        )
        .await;

        proposals.extend(batch_proposals);
        if proposals.len() >= limit as usize {
            break;
        }
    }

    proposals
}

const fn proof_type_label(proof_type: ProofType) -> &'static str {
    match proof_type {
        ProofType::Compressed => "compressed",
        ProofType::SnarkGroth16 => "snark_groth16",
        ProofType::Tee => "tee",
    }
}

const fn tee_kind_label(tee_kind: TeeKind) -> &'static str {
    match tee_kind {
        TeeKind::AwsNitro => "aws_nitro",
    }
}

const fn zk_vm_label(zk_vm: ZkVm) -> &'static str {
    match zk_vm {
        ZkVm::Sp1 => "sp1",
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::{Address, B256};
    use serde_json::json;

    use super::{OnchainProofsReport, ProofsGapReport, ProofsJobStatus, ProofsProposal};

    #[test]
    fn proofs_job_status_parses_and_serializes_snake_case() {
        for (raw, status) in [
            ("queued", ProofsJobStatus::Queued),
            ("running", ProofsJobStatus::Running),
            ("succeeded", ProofsJobStatus::Succeeded),
            ("failed", ProofsJobStatus::Failed),
        ] {
            assert_eq!(ProofsJobStatus::from_str(raw).unwrap(), status);
            assert_eq!(serde_json::to_value(status).unwrap(), json!(raw));
        }

        assert!(ProofsJobStatus::from_str("pending").is_err());
    }

    #[test]
    fn gap_report_uses_saturating_subtraction() {
        let gaps = ProofsGapReport::from_heads(Some(100), Some(200), Some(300), Some(250));

        assert_eq!(gaps.proposer_behind_safe_head, Some(0));
        assert_eq!(gaps.proposer_behind_latest_head, Some(0));
        assert_eq!(gaps.anchor_behind_latest_proposal, Some(0));
        assert_eq!(gaps.anchor_behind_safe_head, Some(0));
    }

    #[test]
    fn report_uses_first_newest_proposal_for_gap_calculation() {
        let proposals = vec![proposal(9, 900), proposal(7, 700), proposal(3, 300)];
        let report = OnchainProofsReport {
            l1_block: Some(100),
            l2_latest_block: Some(1_000),
            l2_safe_block: Some(950),
            l2_finalized_block: Some(800),
            respected_game_type: Some(0),
            system_paused: Some(false),
            total_games: Some(10),
            anchor_l2_block: Some(850),
            anchor_root: Some(B256::ZERO),
            gaps: ProofsGapReport::from_heads(Some(1_000), Some(950), Some(850), Some(900)),
            proposals: proposals.clone(),
        };

        assert_eq!(report.proposals, proposals);
        assert_eq!(report.gaps.proposer_behind_safe_head, Some(50));
        assert_eq!(report.gaps.proposer_behind_latest_head, Some(100));
        assert_eq!(report.gaps.anchor_behind_latest_proposal, Some(50));
        assert_eq!(report.gaps.anchor_behind_safe_head, Some(100));
    }

    fn proposal(factory_index: u64, l2_block: u64) -> ProofsProposal {
        ProofsProposal {
            factory_index,
            game_type: 0,
            game_address: Address::repeat_byte(factory_index as u8),
            l2_block: Some(l2_block),
            root_claim: Some(B256::repeat_byte(factory_index as u8)),
            status: Some(0),
            created_at: 1_780_270_000 + factory_index,
        }
    }
}
