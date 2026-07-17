//! Diagnostic checks and report types for `basectl doctor`.

use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_rpc_types_eth::SyncStatus as EthSyncStatus;
use anyhow::{Context, Result, anyhow};
use base_common_chains::ChainConfig;
use base_consensus_peers::{BootNode, NodeRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{
    ClInfoReport, ElInfoReport, MonitoringConfig, NodeEndpoint, SyncStatusReport, TimestampJson,
    fetch_cl_info, fetch_el_info, fetch_l1_block_number, fetch_l2_block_number, fetch_l2_chain_id,
    fetch_sync_status,
};

const RETH_LIMIT_HARDCAP: u64 = 1024;

/// Diagnostic runner for `basectl doctor`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Doctor;

/// Runtime options for a `basectl doctor` run.
#[derive(Debug, Clone)]
pub struct DoctorOptions {
    /// Execution-layer RPC URL to diagnose as the local node.
    pub el_rpc: Url,
    /// Optional consensus-node RPC URL to diagnose as the local node.
    pub cl_rpc: Option<Url>,
    /// Optional local `reth.toml` path.
    pub reth_config: Option<PathBuf>,
    /// Classification thresholds for graded checks.
    pub thresholds: DoctorThresholds,
}

/// Configurable classification thresholds for `basectl doctor`.
#[derive(Debug, Clone, Copy)]
pub struct DoctorThresholds {
    /// Connected peer count below which peer checks warn.
    pub peer_warn_threshold: u32,
    /// EL head lag above which `el_head_vs_tip` warns.
    pub head_lag_warn_blocks: u64,
    /// EL head lag above which `el_head_vs_tip` fails.
    pub head_lag_fail_blocks: u64,
    /// Safe-head block lag above which `safe_head_recency` warns.
    pub safe_recency_warn_blocks: u64,
    /// Safe-head block lag above which `safe_head_recency` fails.
    pub safe_recency_fail_blocks: u64,
}

impl Default for DoctorThresholds {
    fn default() -> Self {
        Self {
            peer_warn_threshold: 5,
            head_lag_warn_blocks: 10,
            head_lag_fail_blocks: 20,
            safe_recency_warn_blocks: 150,
            safe_recency_fail_blocks: 300,
        }
    }
}

/// Complete humanized `basectl doctor` report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    /// Selected basectl network/config name.
    pub network: String,
    /// Report generation timestamp.
    pub generated_at: TimestampJson,
    /// Effective inputs used by the diagnostic run.
    pub inputs: DoctorInputs,
    /// Count of checks by status.
    pub summary: DoctorSummary,
    /// Individual check results.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Returns `true` if any check failed.
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.status == DoctorStatus::Fail)
    }

    fn new(network: String, inputs: DoctorInputs, checks: Vec<DoctorCheck>) -> Self {
        let summary = DoctorSummary::from_checks(&checks);
        Self { network, generated_at: current_timestamp(), inputs, summary, checks }
    }
}

/// Effective inputs used by a doctor report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorInputs {
    /// Local execution-layer RPC URL.
    pub el_rpc: Url,
    /// Local consensus-node RPC URL, if configured.
    pub cl_rpc: Option<Url>,
    /// Configured L1 RPC URL.
    pub l1_rpc: Url,
    /// Public L2 RPC URL used as the tip reference.
    pub public_tip_rpc: Url,
    /// Optional local `reth.toml` path.
    pub reth_config: Option<PathBuf>,
}

/// Count of doctor checks by status.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DoctorSummary {
    /// Passing graded checks.
    pub pass: usize,
    /// Warning checks.
    pub warn: usize,
    /// Failing checks.
    pub fail: usize,
    /// Informational checks.
    pub info: usize,
    /// Skipped checks.
    pub skip: usize,
}

impl DoctorSummary {
    /// Builds a summary from individual checks.
    pub fn from_checks(checks: &[DoctorCheck]) -> Self {
        let mut summary = Self::default();
        for check in checks {
            match check.status {
                DoctorStatus::Pass => summary.pass += 1,
                DoctorStatus::Warn => summary.warn += 1,
                DoctorStatus::Fail => summary.fail += 1,
                DoctorStatus::Info => summary.info += 1,
                DoctorStatus::Skip => summary.skip += 1,
            }
        }
        summary
    }
}

/// Result of one doctor check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    /// Stable check identifier.
    pub check: String,
    /// Check status.
    pub status: DoctorStatus,
    /// Human-readable diagnostic message.
    pub message: String,
    /// Observed values relevant to the check.
    pub value: Value,
    /// Thresholds relevant to the check.
    pub threshold: Value,
    /// Optional remediation hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl DoctorCheck {
    /// Creates a check result.
    pub fn new(
        check: impl Into<String>,
        status: DoctorStatus,
        message: impl Into<String>,
        value: Value,
        threshold: Value,
        hint: Option<String>,
    ) -> Self {
        Self { check: check.into(), status, message: message.into(), value, threshold, hint }
    }
}

/// Status of a doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    /// The graded check passed.
    Pass,
    /// The check found a risky state but not a hard failure.
    Warn,
    /// The check found a hard failure.
    Fail,
    /// The row is informational context.
    Info,
    /// The check could not run because a required input is unavailable.
    Skip,
}

impl DoctorStatus {
    /// Display label for pretty output.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Info => "INFO",
            Self::Skip => "SKIP",
        }
    }
}

impl Doctor {
    /// Runs doctor checks and returns a complete report.
    pub async fn run(config: MonitoringConfig, options: DoctorOptions) -> DoctorReport {
        let inputs = DoctorInputs {
            el_rpc: options.el_rpc.clone(),
            cl_rpc: options.cl_rpc.clone(),
            l1_rpc: config.l1_rpc.clone(),
            public_tip_rpc: config.rpc.clone(),
            reth_config: options.reth_config.clone(),
        };

        let (el_info, cl_info, chain_id, local_head, public_head, sync_status, l1_head) = tokio::join!(
            fetch_el_info(&options.el_rpc),
            async {
                match &options.cl_rpc {
                    Some(cl_rpc) => Some(fetch_cl_info(cl_rpc).await),
                    None => None,
                }
            },
            fetch_l2_chain_id(&options.el_rpc),
            fetch_l2_block_number(&options.el_rpc),
            fetch_l2_block_number(&config.rpc),
            async {
                match &options.cl_rpc {
                    Some(cl_rpc) => Some(fetch_sync_status(&options.el_rpc, cl_rpc).await),
                    None => None,
                }
            },
            fetch_l1_block_number(&config.l1_rpc),
        );

        let checks = vec![
            Self::p2p_config_check(&el_info, cl_info.as_ref()),
            Self::bootnode_check(chain_id.as_ref().ok(), &config),
            Self::advertised_endpoint_check(&el_info, cl_info.as_ref()),
            Self::declared_network_check(&config, &chain_id),
            Self::reth_limits_check(options.reth_config.as_deref()),
            Self::consensus_rpc_check(options.cl_rpc.as_ref()),
            Self::el_peer_count_check(&el_info, options.thresholds.peer_warn_threshold),
            Self::cl_peer_count_check(cl_info.as_ref(), options.thresholds.peer_warn_threshold),
            Self::head_vs_tip_check(
                &local_head,
                &public_head,
                &options.thresholds,
                &options.el_rpc,
                &config.rpc,
            ),
            Self::safe_head_recency_check(sync_status.as_ref(), &options.thresholds),
            Self::l1_reachability_check(&l1_head, &config.l1_rpc),
        ];

        DoctorReport::new(config.name, inputs, checks)
    }

    fn p2p_config_check(
        el_info: &Result<ElInfoReport>,
        cl_info: Option<&Result<ClInfoReport>>,
    ) -> DoctorCheck {
        let value = json!({
            "el": layer_config_value(el_info.as_ref().ok().and_then(|info| info.endpoint), el_info.as_ref().ok().and_then(|info| info.peer_count), el_info.as_ref().err()),
            "cl": match cl_info {
                Some(Ok(info)) => layer_config_value(info.endpoint, info.peer_stats.map(|stats| stats.connected), None),
                Some(Err(err)) => layer_config_value(None, None, Some(err)),
                None => json!({ "available": false, "reason": "consensus-node RPC unavailable" }),
            },
        });
        DoctorCheck::new(
            "p2p_config",
            DoctorStatus::Info,
            "p2p advertised endpoint and peer-count context",
            value,
            json!(null),
            Some("Endpoint data is reported from local RPCs and does not prove public inbound reachability.".to_string()),
        )
    }

    fn bootnode_check(live_chain_id: Option<&u64>, config: &MonitoringConfig) -> DoctorCheck {
        let expected = expected_chain_id(&config.name);
        let chain = bootnode_chain(&config.name, live_chain_id.copied());
        let Some(chain) = chain else {
            return DoctorCheck::new(
                "bootnode_config",
                DoctorStatus::Skip,
                "canonical bootnodes are unavailable for this config",
                json!({ "network": config.name }),
                json!(null),
                Some("Use a known Base network config to report canonical bootnodes.".to_string()),
            );
        };
        if chain.bootnodes.total() == 0 {
            return DoctorCheck::new(
                "bootnode_config",
                DoctorStatus::Skip,
                "canonical bootnode list is empty for this network",
                json!({ "chainId": chain.chain_id }),
                json!(null),
                None,
            );
        }

        DoctorCheck::new(
            "bootnode_config",
            DoctorStatus::Info,
            "canonical bootnodes for selected network; reachability is not verified by doctor v1",
            json!({
                "network": config.name,
                "chainId": chain.chain_id,
                "declaredChainId": expected,
                "liveChainId": live_chain_id,
                "execution": bootnode_layer_summary(chain.bootnodes.execution),
                "consensus": bootnode_layer_summary(chain.bootnodes.consensus),
            }),
            json!(null),
            Some("Bootnodes help cold-start peer discovery. Current peer health is graded by `el_peer_count` and `cl_peer_count`.".to_string()),
        )
    }

    fn advertised_endpoint_check(
        el_info: &Result<ElInfoReport>,
        cl_info: Option<&Result<ClInfoReport>>,
    ) -> DoctorCheck {
        let el = endpoint_sanity(el_info.as_ref().ok().and_then(|info| info.endpoint));
        let cl = match cl_info {
            Some(Ok(info)) => endpoint_sanity(info.endpoint),
            Some(Err(err)) => unavailable_endpoint(err.to_string()),
            None => unavailable_endpoint("consensus-node RPC unavailable"),
        };
        let status = worst_status([el.status, cl.status]);
        DoctorCheck::new(
            "advertised_endpoint_sanity",
            status,
            "advertised endpoint IP sanity from exposed node metadata",
            json!({ "el": el, "cl": cl }),
            json!({ "fail": "unspecified or loopback IP", "warn": "private or link-local IP" }),
            Some("This check does not use an external observer and cannot prove public inbound reachability.".to_string()),
        )
    }

    fn declared_network_check(config: &MonitoringConfig, chain_id: &Result<u64>) -> DoctorCheck {
        let expected = expected_chain_id(&config.name);
        match (expected, chain_id) {
            (_, Err(err)) => DoctorCheck::new(
                "declared_network",
                DoctorStatus::Fail,
                "could not fetch live chain ID from the local EL RPC",
                json!({ "network": config.name, "error": err.to_string() }),
                json!(null),
                Some("Check `--el-rpc` and the selected `-c/--config` value.".to_string()),
            ),
            (Some(expected), Ok(actual)) if expected == *actual => DoctorCheck::new(
                "declared_network",
                DoctorStatus::Pass,
                "declared network matches live chain ID",
                json!({ "network": config.name, "chainId": actual, "expectedChainId": expected }),
                json!({ "expectedChainId": expected }),
                None,
            ),
            (Some(expected), Ok(actual)) => DoctorCheck::new(
                "declared_network",
                DoctorStatus::Fail,
                "declared network does not match live chain ID",
                json!({ "network": config.name, "chainId": actual, "expectedChainId": expected }),
                json!({ "expectedChainId": expected }),
                Some("Point `--el-rpc` at a node for the selected config, or select the matching config with `-c`.".to_string()),
            ),
            (None, Ok(actual)) => DoctorCheck::new(
                "declared_network",
                DoctorStatus::Warn,
                "selected config has no known expected chain ID",
                json!({ "network": config.name, "chainId": actual }),
                json!(null),
                None,
            ),
        }
    }

    fn reth_limits_check(path: Option<&Path>) -> DoctorCheck {
        let Some(path) = path else {
            return DoctorCheck::new(
                "reth_limits",
                DoctorStatus::Skip,
                "reth config path was not provided",
                json!({ "path": null }),
                json!({ "failAtOrAbove": RETH_LIMIT_HARDCAP }),
                Some(
                    "Pass `--reth-config <PATH>` to check `headers.limit` and `bodies.limit`."
                        .to_string(),
                ),
            );
        };

        match RethLimits::read(path) {
            Ok(limits) => limits.into_check(path),
            Err(err) => DoctorCheck::new(
                "reth_limits",
                DoctorStatus::Warn,
                "could not read or parse reth config limits",
                json!({ "path": path, "error": err.to_string() }),
                json!({ "failAtOrAbove": RETH_LIMIT_HARDCAP }),
                Some("Confirm the path points to a readable `reth.toml`.".to_string()),
            ),
        }
    }

    fn consensus_rpc_check(cl_rpc: Option<&Url>) -> DoctorCheck {
        cl_rpc.map_or_else(
            || {
                DoctorCheck::new(
                    "consensus_node_rpc",
                    DoctorStatus::Warn,
                    "consensus-node RPC is not configured",
                    json!({ "clRpc": null }),
                    json!(null),
                    Some("Pass `--cl-rpc <URL>` or set `consensus_node_rpc` in the selected YAML config to run CL checks.".to_string()),
                )
            },
            |url| DoctorCheck::new(
                "consensus_node_rpc",
                DoctorStatus::Pass,
                "consensus-node RPC is configured",
                json!({ "clRpc": url }),
                json!(null),
                None,
            ),
        )
    }

    fn el_peer_count_check(el_info: &Result<ElInfoReport>, warn_threshold: u32) -> DoctorCheck {
        match el_info {
            Ok(info) => info.peer_count.map_or_else(
                || {
                    DoctorCheck::new(
                        "el_peer_count",
                        DoctorStatus::Skip,
                        "could not fetch EL peer count",
                        json!({ "error": info.peer_count_error }),
                        json!({ "failAt": 0, "warnBelow": warn_threshold }),
                        Some("Check `--el-rpc` and ensure `net_peerCount` is exposed.".to_string()),
                    )
                },
                |count| peer_count_check("el_peer_count", "EL", count, warn_threshold),
            ),
            Err(err) => DoctorCheck::new(
                "el_peer_count",
                DoctorStatus::Fail,
                "could not fetch EL peer count",
                json!({ "error": err.to_string() }),
                json!({ "failAt": 0, "warnBelow": warn_threshold }),
                Some("Check `--el-rpc` and ensure `net_peerCount` is exposed.".to_string()),
            ),
        }
    }

    fn cl_peer_count_check(
        cl_info: Option<&Result<ClInfoReport>>,
        warn_threshold: u32,
    ) -> DoctorCheck {
        match cl_info {
            Some(Ok(info)) => info.peer_stats.map_or_else(
                || {
                    DoctorCheck::new(
                        "cl_peer_count",
                        DoctorStatus::Skip,
                        "CL peer-count RPC method is unavailable",
                        json!({ "peerStats": null, "error": info.peer_stats_error }),
                        json!({ "failAt": 0, "warnBelow": warn_threshold }),
                        Some(
                            "Ensure the consensus-node RPC exposes `opp2p_peerStats`.".to_string(),
                        ),
                    )
                },
                |stats| peer_count_check("cl_peer_count", "CL", stats.connected, warn_threshold),
            ),
            Some(Err(err)) => DoctorCheck::new(
                "cl_peer_count",
                DoctorStatus::Fail,
                "could not fetch CL peer count",
                json!({ "error": err.to_string() }),
                json!({ "failAt": 0, "warnBelow": warn_threshold }),
                Some("Check `--cl-rpc` and ensure `opp2p_peerStats` is exposed.".to_string()),
            ),
            None => DoctorCheck::new(
                "cl_peer_count",
                DoctorStatus::Skip,
                "consensus-node RPC is unavailable",
                json!({ "clRpc": null }),
                json!({ "failAt": 0, "warnBelow": warn_threshold }),
                Some(
                    "Pass `--cl-rpc <URL>` or set `consensus_node_rpc` in YAML config.".to_string(),
                ),
            ),
        }
    }

    fn head_vs_tip_check(
        local_head: &Result<u64>,
        public_head: &Result<u64>,
        thresholds: &DoctorThresholds,
        local_rpc: &Url,
        public_rpc: &Url,
    ) -> DoctorCheck {
        let threshold = json!({
            "warnBehindBlocks": thresholds.head_lag_warn_blocks,
            "failBehindBlocks": thresholds.head_lag_fail_blocks,
        });
        if local_rpc == public_rpc {
            return DoctorCheck::new(
                "el_head_vs_tip",
                DoctorStatus::Skip,
                "local EL RPC matches the public tip reference",
                json!({ "localRpc": local_rpc, "publicTipRpc": public_rpc }),
                threshold,
                Some("Pass `--el-rpc <URL>` pointing at a specific node to run a meaningful head-vs-tip check.".to_string()),
            );
        }
        let Ok(local) = local_head else {
            return DoctorCheck::new(
                "el_head_vs_tip",
                DoctorStatus::Fail,
                "could not fetch local EL head",
                json!({ "error": local_head.as_ref().err().map(ToString::to_string) }),
                threshold,
                Some("Check `--el-rpc` and local EL availability.".to_string()),
            );
        };
        let Ok(public) = public_head else {
            return DoctorCheck::new(
                "el_head_vs_tip",
                DoctorStatus::Warn,
                "could not fetch public tip reference",
                json!({
                    "localBlockNumber": local,
                    "publicTipRpc": public_rpc,
                    "error": public_head.as_ref().err().map(ToString::to_string),
                }),
                threshold,
                Some(
                    "Public tip comparison is unavailable; retry or check network connectivity."
                        .to_string(),
                ),
            );
        };

        let delta = i128::from(*public) - i128::from(*local);
        let status = if delta < 0 {
            DoctorStatus::Warn
        } else if delta > i128::from(thresholds.head_lag_fail_blocks) {
            DoctorStatus::Fail
        } else if delta > i128::from(thresholds.head_lag_warn_blocks) {
            DoctorStatus::Warn
        } else {
            DoctorStatus::Pass
        };
        let message = match status {
            DoctorStatus::Pass => "local EL head is close to the public tip",
            DoctorStatus::Warn if delta < 0 => "local EL head is ahead of the public tip reference",
            DoctorStatus::Warn => "local EL head is behind the public tip",
            DoctorStatus::Fail => "local EL head is far behind the public tip",
            _ => "EL head comparison completed",
        };
        DoctorCheck::new(
            "el_head_vs_tip",
            status,
            message,
            json!({
                "localBlockNumber": local,
                "publicBlockNumber": public,
                "deltaBlocks": delta,
                "publicTipRpc": public_rpc,
            }),
            threshold,
            None,
        )
    }

    fn safe_head_recency_check(
        sync_status: Option<&Result<SyncStatusReport>>,
        thresholds: &DoctorThresholds,
    ) -> DoctorCheck {
        let threshold = json!({
            "warnBlocks": thresholds.safe_recency_warn_blocks,
            "failBlocks": thresholds.safe_recency_fail_blocks,
        });
        let Some(sync_status) = sync_status else {
            return DoctorCheck::new(
                "safe_head_recency",
                DoctorStatus::Skip,
                "consensus-node RPC is unavailable",
                json!({ "clRpc": null }),
                threshold,
                Some(
                    "Pass `--cl-rpc <URL>` or set `consensus_node_rpc` in YAML config.".to_string(),
                ),
            );
        };
        let Ok(report) = sync_status else {
            return DoctorCheck::new(
                "safe_head_recency",
                DoctorStatus::Fail,
                "could not fetch sync status",
                json!({ "error": sync_status.as_ref().err().map(ToString::to_string) }),
                threshold,
                Some("Check `--el-rpc`, `--cl-rpc`, `eth_syncing`, and `optimism_syncStatus` availability.".to_string()),
            );
        };
        if matches!(report.el, EthSyncStatus::Info(_)) {
            return DoctorCheck::new(
                "safe_head_recency",
                DoctorStatus::Skip,
                "EL is actively syncing, so safe-head recency is not graded",
                json!({ "elActivelySyncing": true }),
                threshold,
                None,
            );
        }

        let unsafe_number = report.cl.unsafe_l2.block_info.number;
        let safe_number = report.cl.safe_l2.block_info.number;
        let lag_blocks = unsafe_number.saturating_sub(safe_number);
        let lag_seconds = report
            .cl
            .unsafe_l2
            .block_info
            .timestamp
            .saturating_sub(report.cl.safe_l2.block_info.timestamp);
        let status = if lag_blocks > thresholds.safe_recency_fail_blocks {
            DoctorStatus::Fail
        } else if lag_blocks > thresholds.safe_recency_warn_blocks {
            DoctorStatus::Warn
        } else {
            DoctorStatus::Pass
        };
        let message = match status {
            DoctorStatus::Pass => "safe head is close to unsafe head",
            DoctorStatus::Warn => "safe head is lagging unsafe head",
            DoctorStatus::Fail => "safe head is far behind unsafe head",
            _ => "safe-head recency check completed",
        };
        DoctorCheck::new(
            "safe_head_recency",
            status,
            message,
            json!({
                "unsafeBlockNumber": unsafe_number,
                "safeBlockNumber": safe_number,
                "lagBlocks": lag_blocks,
                "lagSeconds": lag_seconds,
                "unsafeTimestamp": TimestampJson::from_unix(report.cl.unsafe_l2.block_info.timestamp),
                "safeTimestamp": TimestampJson::from_unix(report.cl.safe_l2.block_info.timestamp),
            }),
            threshold,
            Some("Some sync modes intentionally keep safe behind unsafe; adjust thresholds if this is expected for your node.".to_string()),
        )
    }

    fn l1_reachability_check(l1_head: &Result<u64>, l1_rpc: &Url) -> DoctorCheck {
        match l1_head {
            Ok(block_number) => DoctorCheck::new(
                "l1_reachability",
                DoctorStatus::Pass,
                "configured L1 RPC is reachable",
                json!({ "l1Rpc": l1_rpc, "blockNumber": block_number }),
                json!(null),
                None,
            ),
            Err(err) => DoctorCheck::new(
                "l1_reachability",
                DoctorStatus::Fail,
                "configured L1 RPC is unreachable",
                json!({ "l1Rpc": l1_rpc, "error": err.to_string() }),
                json!(null),
                Some(
                    "Check the selected config's `l1_rpc` value and network connectivity."
                        .to_string(),
                ),
            ),
        }
    }
}

/// Per-layer advertised endpoint sanity classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerEndpointSanity {
    /// Layer-specific endpoint status.
    pub status: DoctorStatus,
    /// Advertised IP address, when available.
    pub advertised_ip: Option<IpAddr>,
    /// Advertised TCP p2p port, when available.
    pub tcp_port: Option<u16>,
    /// Advertised UDP discovery port, when available.
    pub discovery_udp_port: Option<u16>,
    /// Human-readable classification reason.
    pub reason: String,
}

/// Minimal `reth.toml` shape needed by doctor.
#[derive(Debug, Deserialize)]
pub struct RethToml {
    /// Header download limit section.
    pub headers: Option<RethLimitSection>,
    /// Body download limit section.
    pub bodies: Option<RethLimitSection>,
}

/// Minimal reth limit section shape.
#[derive(Debug, Deserialize)]
pub struct RethLimitSection {
    /// Configured request limit.
    pub limit: Option<u64>,
}

/// Parsed reth headers/bodies limits.
#[derive(Debug, Clone, Copy)]
pub struct RethLimits {
    /// Parsed `headers.limit` value.
    pub headers: Option<u64>,
    /// Parsed `bodies.limit` value.
    pub bodies: Option<u64>,
}

impl RethLimits {
    /// Reads the minimal reth limits from a TOML file.
    pub fn read(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading reth config at {}", path.display()))?;
        let parsed: RethToml = toml::from_str(&contents)
            .with_context(|| format!("parsing reth config at {}", path.display()))?;
        Ok(Self {
            headers: parsed.headers.and_then(|section| section.limit),
            bodies: parsed.bodies.and_then(|section| section.limit),
        })
    }

    /// Converts parsed limits into a doctor check.
    pub fn into_check(self, path: &Path) -> DoctorCheck {
        let missing = [
            self.headers.is_none().then_some("headers.limit"),
            self.bodies.is_none().then_some("bodies.limit"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            return DoctorCheck::new(
                "reth_limits",
                DoctorStatus::Warn,
                "reth config is missing required limit field(s)",
                json!({ "path": path, "headersLimit": self.headers, "bodiesLimit": self.bodies, "missing": missing }),
                json!({ "failAtOrAbove": RETH_LIMIT_HARDCAP }),
                Some("Set both `headers.limit` and `bodies.limit` below 1024.".to_string()),
            );
        }
        let headers = self.headers.unwrap_or_default();
        let bodies = self.bodies.unwrap_or_default();
        let status = if headers >= RETH_LIMIT_HARDCAP || bodies >= RETH_LIMIT_HARDCAP {
            DoctorStatus::Fail
        } else {
            DoctorStatus::Pass
        };
        let message = match status {
            DoctorStatus::Pass => "reth headers/bodies limits are below the hardcap",
            DoctorStatus::Fail => "reth headers/bodies limits are at or above the hardcap",
            _ => "reth headers/bodies limits were checked",
        };
        DoctorCheck::new(
            "reth_limits",
            status,
            message,
            json!({ "path": path, "headersLimit": headers, "bodiesLimit": bodies }),
            json!({ "failAtOrAbove": RETH_LIMIT_HARDCAP }),
            (status == DoctorStatus::Fail)
                .then(|| "Set both `headers.limit` and `bodies.limit` below 1024.".to_string()),
        )
    }
}

fn current_timestamp() -> TimestampJson {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    TimestampJson::from_unix(secs)
}

fn layer_config_value(
    endpoint: Option<NodeEndpoint>,
    peer_count: Option<u32>,
    error: Option<&anyhow::Error>,
) -> Value {
    endpoint.map_or_else(
        || json!({
            "available": false,
            "peerCount": peer_count,
            "reason": error.map(ToString::to_string).unwrap_or_else(|| "advertised endpoint unavailable".to_string()),
        }),
        |endpoint| json!({
            "available": true,
            "advertisedIp": endpoint.advertised_ip,
            "tcpPort": endpoint.tcp_port,
            "discoveryUdpPort": endpoint.discovery.udp_port,
            "discv4": endpoint.discovery.v4_enabled,
            "discv5": endpoint.discovery.v5_enabled,
            "peerCount": peer_count,
        }),
    )
}

fn bootnode_layer_summary(raw_bootnodes: &[&str]) -> Value {
    let mut parsed = 0;
    let mut parse_failed = 0;
    let mut enode = 0;
    let mut enr = 0;
    let mut tcp_ports = BTreeMap::<u16, usize>::new();
    let mut udp_ports = BTreeMap::<u16, usize>::new();

    for raw in raw_bootnodes {
        match bootnode_addrs(raw) {
            Ok((kind, tcp, udp)) => {
                parsed += 1;
                match kind {
                    "enode" => enode += 1,
                    "enr" => enr += 1,
                    _ => {}
                }
                if let Some(addr) = tcp {
                    *tcp_ports.entry(addr.port()).or_default() += 1;
                }
                if let Some(addr) = udp {
                    *udp_ports.entry(addr.port()).or_default() += 1;
                }
            }
            Err(_) => parse_failed += 1,
        }
    }

    json!({
        "total": raw_bootnodes.len(),
        "parsed": parsed,
        "parseFailed": parse_failed,
        "enode": enode,
        "enr": enr,
        "tcpPorts": tcp_ports,
        "udpPorts": udp_ports,
    })
}

fn bootnode_addrs(raw: &str) -> Result<(&'static str, Option<SocketAddr>, Option<SocketAddr>)> {
    if raw.starts_with("enode://") {
        let record =
            NodeRecord::from_str(raw).with_context(|| format!("parsing bootnode `{raw}`"))?;
        return Ok(("enode", Some(record.tcp_addr()), Some(record.udp_addr())));
    }

    match BootNode::parse_bootnode(raw)? {
        BootNode::Enode(_) => Err(anyhow!("unsupported non-enode:// Enode bootnode format")),
        BootNode::Enr(enr) => {
            let ip = enr
                .ip4()
                .map(IpAddr::V4)
                .or_else(|| enr.ip6().map(IpAddr::V6))
                .ok_or_else(|| anyhow!("ENR bootnode has no IP address"))?;
            Ok((
                "enr",
                enr.tcp4().or_else(|| enr.tcp6()).map(|port| SocketAddr::new(ip, port)),
                enr.udp4().or_else(|| enr.udp6()).map(|port| SocketAddr::new(ip, port)),
            ))
        }
    }
}

fn endpoint_sanity(endpoint: Option<NodeEndpoint>) -> LayerEndpointSanity {
    let Some(endpoint) = endpoint else {
        return unavailable_endpoint("advertised endpoint unavailable");
    };
    let (status, reason) = classify_ip(endpoint.advertised_ip);
    LayerEndpointSanity {
        status,
        advertised_ip: Some(endpoint.advertised_ip),
        tcp_port: Some(endpoint.tcp_port),
        discovery_udp_port: Some(endpoint.discovery.udp_port),
        reason,
    }
}

fn unavailable_endpoint(reason: impl Into<String>) -> LayerEndpointSanity {
    LayerEndpointSanity {
        status: DoctorStatus::Skip,
        advertised_ip: None,
        tcp_port: None,
        discovery_udp_port: None,
        reason: reason.into(),
    }
}

fn classify_ip(ip: IpAddr) -> (DoctorStatus, String) {
    if ip.is_unspecified() || ip.is_loopback() {
        return (DoctorStatus::Fail, "advertised IP is unspecified or loopback".to_string());
    }
    match ip {
        IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => {
            (DoctorStatus::Warn, "advertised IP is private or link-local".to_string())
        }
        IpAddr::V6(v6) if v6.is_unique_local() || v6.is_unicast_link_local() => {
            (DoctorStatus::Warn, "advertised IP is private or link-local".to_string())
        }
        _ => (DoctorStatus::Pass, "advertised IP is public-looking".to_string()),
    }
}

fn worst_status(statuses: impl IntoIterator<Item = DoctorStatus>) -> DoctorStatus {
    let statuses = statuses.into_iter().collect::<Vec<_>>();
    if statuses.contains(&DoctorStatus::Fail) {
        DoctorStatus::Fail
    } else if statuses.contains(&DoctorStatus::Warn) {
        DoctorStatus::Warn
    } else if statuses.contains(&DoctorStatus::Pass) {
        DoctorStatus::Pass
    } else if statuses.contains(&DoctorStatus::Info) {
        DoctorStatus::Info
    } else {
        DoctorStatus::Skip
    }
}

fn expected_chain_id(network: &str) -> Option<u64> {
    match network {
        "mainnet" => Some(8453),
        "sepolia" => Some(84532),
        "devnet" => Some(1337),
        "zeronet" => Some(763360),
        _ => None,
    }
}

fn bootnode_chain(network: &str, live_chain_id: Option<u64>) -> Option<&'static ChainConfig> {
    expected_chain_id(network)
        .and_then(ChainConfig::by_chain_id)
        .or_else(|| live_chain_id.and_then(ChainConfig::by_chain_id))
}

fn peer_count_check(check: &str, layer: &str, count: u32, warn_threshold: u32) -> DoctorCheck {
    let status = if count == 0 {
        DoctorStatus::Fail
    } else if count < warn_threshold {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Pass
    };
    let message = match status {
        DoctorStatus::Pass => format!("{layer} peer count is healthy"),
        DoctorStatus::Warn => format!("{layer} peer count is below the warning threshold"),
        DoctorStatus::Fail => format!("{layer} peer count is zero"),
        _ => format!("{layer} peer count check completed"),
    };
    DoctorCheck::new(
        check,
        status,
        message,
        json!({ "count": count }),
        json!({ "failAt": 0, "warnBelow": warn_threshold }),
        (status != DoctorStatus::Pass)
            .then(|| "Check firewall, NAT, and advertised endpoint configuration.".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        path::PathBuf,
    };

    use serde_json::json;
    use url::Url;

    use super::{
        Doctor, DoctorCheck, DoctorStatus, DoctorSummary, DoctorThresholds, ElInfoReport,
        RethLimits, bootnode_addrs, bootnode_chain, bootnode_layer_summary, classify_ip,
        expected_chain_id, peer_count_check, worst_status,
    };
    use crate::ElNodeIdentity;

    #[test]
    fn summary_counts_all_statuses() {
        let checks = vec![
            check(DoctorStatus::Pass),
            check(DoctorStatus::Warn),
            check(DoctorStatus::Fail),
            check(DoctorStatus::Info),
            check(DoctorStatus::Skip),
        ];

        let summary = DoctorSummary::from_checks(&checks);

        assert_eq!(summary.pass, 1);
        assert_eq!(summary.warn, 1);
        assert_eq!(summary.fail, 1);
        assert_eq!(summary.info, 1);
        assert_eq!(summary.skip, 1);
    }

    #[test]
    fn peer_thresholds_classify_zero_warn_and_pass() {
        assert_eq!(peer_count_check("x", "EL", 0, 5).status, DoctorStatus::Fail);
        assert_eq!(peer_count_check("x", "EL", 4, 5).status, DoctorStatus::Warn);
        assert_eq!(peer_count_check("x", "EL", 5, 5).status, DoctorStatus::Pass);
    }

    #[test]
    fn default_thresholds_match_doctor_plan() {
        let thresholds = DoctorThresholds::default();

        assert_eq!(thresholds.head_lag_warn_blocks, 10);
        assert_eq!(thresholds.head_lag_fail_blocks, 20);
        assert_eq!(thresholds.safe_recency_warn_blocks, 150);
        assert_eq!(thresholds.safe_recency_fail_blocks, 300);
    }

    #[test]
    fn classifies_advertised_ips() {
        assert_eq!(classify_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)).0, DoctorStatus::Fail);
        assert_eq!(classify_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).0, DoctorStatus::Fail);
        assert_eq!(classify_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))).0, DoctorStatus::Warn);
        assert_eq!(classify_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)).0, DoctorStatus::Fail);
        assert_eq!(classify_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).0, DoctorStatus::Pass);
    }

    #[test]
    fn reth_limits_classify_values() {
        let path = PathBuf::from("reth.toml");

        assert_eq!(
            RethLimits { headers: Some(100), bodies: Some(100) }.into_check(&path).status,
            DoctorStatus::Pass,
        );
        assert_eq!(
            RethLimits { headers: Some(1024), bodies: Some(100) }.into_check(&path).status,
            DoctorStatus::Fail,
        );
        assert_eq!(
            RethLimits { headers: None, bodies: Some(100) }.into_check(&path).status,
            DoctorStatus::Warn,
        );
    }

    #[test]
    fn head_vs_tip_skips_when_local_rpc_is_public_reference() {
        let rpc = Url::parse("https://sepolia.base.org").unwrap();
        let thresholds = DoctorThresholds::default();

        let check = Doctor::head_vs_tip_check(&Ok(10), &Ok(10), &thresholds, &rpc, &rpc);

        assert_eq!(check.status, DoctorStatus::Skip);
    }

    #[test]
    fn el_peer_count_unavailable_skips_instead_of_failing() {
        let info = Ok(ElInfoReport {
            endpoint: None,
            identity: ElNodeIdentity::default(),
            peer_count: None,
            peer_count_error: Some("EL `net_peerCount` unavailable from this RPC"),
        });

        let check = Doctor::el_peer_count_check(&info, 5);

        assert_eq!(check.status, DoctorStatus::Skip);
    }

    #[test]
    fn maps_known_config_names_to_chain_ids() {
        assert_eq!(expected_chain_id("mainnet"), Some(8453));
        assert_eq!(expected_chain_id("sepolia"), Some(84532));
        assert_eq!(expected_chain_id("devnet"), Some(1337));
        assert_eq!(expected_chain_id("zeronet"), Some(763360));
        assert_eq!(expected_chain_id("custom"), None);
    }

    #[test]
    fn enr_bootnode_parser_preserves_udp_address() {
        let enr = "enr:-J64QBbwPjPLZ6IOOToOLsSjtFUjjzN66qmBZdUexpO32Klrc458Q24kbty2PdRaLacHM5z-cZQr8mjeQu3pik6jPSOGAYYFIqBfgmlkgnY0gmlwhDaRWFWHb3BzdGFja4SzlAUAiXNlY3AyNTZrMaECmeSnJh7zjKrDSPoNMGXoopeDF4hhpj5I0OsQUUt4u8uDdGNwgiQGg3VkcIIkBg";

        let (_, _, udp) = bootnode_addrs(enr).unwrap();

        assert!(udp.is_some());
    }

    #[test]
    fn bootnode_summary_reports_counts_without_reachability() {
        let summary = bootnode_layer_summary(&[
            "enode://d7dfaea49c7ef37701e668652bcf1bc63d3abb2ae97593374a949e175e4ff128730a2f35199f3462a56298b981dfc395a5abebd2d6f0284ffe5bdc3d8e258b86@127.0.0.1:30304?discport=30301",
            "not-a-bootnode",
        ]);

        assert_eq!(summary["total"], 2);
        assert_eq!(summary["parsed"], 1);
        assert_eq!(summary["parseFailed"], 1);
        assert_eq!(summary["enode"], 1);
        assert_eq!(summary["tcpPorts"]["30304"], 1);
        assert_eq!(summary["udpPorts"]["30301"], 1);
    }

    #[test]
    fn advertised_endpoint_status_uses_checked_layer_over_skip() {
        assert_eq!(worst_status([DoctorStatus::Pass, DoctorStatus::Skip]), DoctorStatus::Pass);
        assert_eq!(worst_status([DoctorStatus::Warn, DoctorStatus::Skip]), DoctorStatus::Warn);
        assert_eq!(worst_status([DoctorStatus::Fail, DoctorStatus::Skip]), DoctorStatus::Fail);
        assert_eq!(worst_status([DoctorStatus::Skip, DoctorStatus::Skip]), DoctorStatus::Skip);
        assert_eq!(worst_status([DoctorStatus::Pass, DoctorStatus::Warn]), DoctorStatus::Warn);
    }

    #[test]
    fn bootnode_chain_prefers_declared_config_over_live_chain_id() {
        let chain = bootnode_chain("mainnet", Some(84532)).unwrap();

        assert_eq!(chain.chain_id, 8453);
    }

    #[test]
    fn bootnode_chain_falls_back_to_live_chain_for_unknown_config() {
        let chain = bootnode_chain("custom", Some(84532)).unwrap();

        assert_eq!(chain.chain_id, 84532);
    }

    fn check(status: DoctorStatus) -> DoctorCheck {
        DoctorCheck::new("check", status, "message", json!(null), json!(null), None)
    }
}
