//! Implementation of the `basectl sync-status` subcommand.

use std::time::Duration;

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{BlockNumberOrTag, SyncStatus as EthSyncStatus};
use anyhow::Result;
use base_protocol::{BlockInfo, L2BlockInfo};
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, SyncStatusCommandError, SyncStatusReport,
    TimestampJson, fetch_block, fetch_sync_status, format_duration, format_unix_timestamp,
};
use serde::Serialize;
use url::Url;

/// Runs the `basectl sync-status` subcommand.
pub(crate) async fn run(
    config: MonitoringConfig,
    el_rpc_override: Option<Url>,
    cl_rpc_override: Option<Url>,
    tip_tolerance: u64,
    json: bool,
    raw: bool,
) -> Result<()> {
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref())?;
    // Public tip reference is best-effort — failure marks the row unavailable
    // rather than failing the whole command. Run in parallel with the local
    // sync fetch.
    let (sync_result, tip_result) = tokio::join!(
        fetch_sync_status(&el_rpc, &cl_rpc),
        fetch_block(&config.rpc, BlockId::Number(BlockNumberOrTag::Latest)),
    );
    let report = sync_result?;
    let public_tip_block = tip_result.ok().map(|b| b.header.number);
    let tip_url = config.rpc.as_str();

    match (json, raw) {
        (true, true) => JsonOutput::print(&report.cl)?,
        (true, false) => {
            let summary = SyncStatusJson::from_report(
                &config.name,
                &report,
                tip_url,
                public_tip_block,
                tip_tolerance,
            );
            JsonOutput::print(&summary)?;
        }
        (false, _) => {
            print_pretty(&config.name, &report, tip_url, public_tip_block, tip_tolerance)?;
        }
    }
    Ok(())
}

/// Resolves the consensus-node RPC URL with precedence:
/// `--cl-rpc` flag → `MonitoringConfig.consensus_node_rpc` → clear error.
///
/// The mainnet and sepolia presets ship `consensus_node_rpc: None`, so
/// non-devnet users must supply the URL explicitly.
fn resolve_cl_rpc(
    config: &MonitoringConfig,
    override_url: Option<&Url>,
) -> Result<Url, SyncStatusCommandError> {
    if let Some(u) = override_url {
        return Ok(u.clone());
    }
    config.consensus_node_rpc.clone().ok_or_else(|| SyncStatusCommandError::MissingConsensusRpc {
        config_name: config.name.clone(),
    })
}

/// Humanized JSON shape for `basectl sync-status --json`.
///
/// Decoded numerics, nested timestamp objects, a precomputed `safeLag*`
/// pair, and a `tipReference` block for the public-RPC comparison so
/// consumers don't have to re-derive any of these from raw fields.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncStatusJson {
    network: String,
    el_actively_syncing: bool,
    el_sync_info: Option<ElSyncInfoJson>,
    unsafe_l2: HeadJson,
    safe_l2: HeadJson,
    finalized_l2: HeadJson,
    safe_lag_seconds: u64,
    safe_lag_blocks: u64,
    l1_head: HeadJson,
    l1_safe: HeadJson,
    l1_finalized: HeadJson,
    tip_reference: TipReferenceJson,
}

impl SyncStatusJson {
    fn from_report(
        network: &str,
        report: &SyncStatusReport,
        tip_url: &str,
        public_tip_block: Option<u64>,
        tip_tolerance: u64,
    ) -> Self {
        let cl = &report.cl;
        let safe_lag_seconds =
            cl.unsafe_l2.block_info.timestamp.saturating_sub(cl.safe_l2.block_info.timestamp);
        let safe_lag_blocks =
            cl.unsafe_l2.block_info.number.saturating_sub(cl.safe_l2.block_info.number);
        let (el_actively_syncing, el_sync_info) = match &report.el {
            EthSyncStatus::Info(info) => {
                let starting = info.starting_block.to::<u64>();
                let current = info.current_block.to::<u64>();
                let highest = info.highest_block.to::<u64>();
                (
                    true,
                    Some(ElSyncInfoJson {
                        starting_block: starting,
                        current_block: current,
                        highest_block: highest,
                        processed_blocks: current.saturating_sub(starting),
                        remaining_blocks: highest.saturating_sub(current),
                    }),
                )
            }
            EthSyncStatus::None => (false, None),
        };
        let tip_reference = TipReferenceJson::from_local_and_public(
            tip_url,
            cl.unsafe_l2.block_info.number,
            public_tip_block,
            tip_tolerance,
        );
        Self {
            network: network.to_string(),
            el_actively_syncing,
            el_sync_info,
            unsafe_l2: HeadJson::from_l2(&cl.unsafe_l2),
            safe_l2: HeadJson::from_l2(&cl.safe_l2),
            finalized_l2: HeadJson::from_l2(&cl.finalized_l2),
            safe_lag_seconds,
            safe_lag_blocks,
            l1_head: HeadJson::from_l1(&cl.head_l1),
            l1_safe: HeadJson::from_l1(&cl.safe_l1),
            l1_finalized: HeadJson::from_l1(&cl.finalized_l1),
            tip_reference,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HeadJson {
    number: u64,
    hash: B256,
    timestamp: TimestampJson,
}

impl HeadJson {
    fn from_l2(b: &L2BlockInfo) -> Self {
        Self::from_block_info(&b.block_info)
    }

    fn from_l1(b: &BlockInfo) -> Self {
        Self::from_block_info(b)
    }

    fn from_block_info(b: &BlockInfo) -> Self {
        Self { number: b.number, hash: b.hash, timestamp: TimestampJson::from_unix(b.timestamp) }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ElSyncInfoJson {
    starting_block: u64,
    current_block: u64,
    highest_block: u64,
    /// Blocks processed since EL sync began (`current - starting`).
    processed_blocks: u64,
    /// Blocks still to process before EL sync completes (`highest - current`).
    remaining_blocks: u64,
}

/// Comparison of the local node's unsafe L2 head against a public-RPC
/// reference (`config.rpc` for the active preset). Best-effort — when the
/// public fetch fails, `block_number` and `delta_blocks` are `None` and
/// `status` is `unavailable`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TipReferenceJson {
    /// The public-RPC URL queried (the preset's `config.rpc`).
    url: String,
    /// Latest block number reported by the public RPC. `None` if the call failed.
    block_number: Option<u64>,
    /// Signed delta `public - local`. Positive means local is behind; negative
    /// means local is ahead. `None` if the public block isn't known.
    delta_blocks: Option<i64>,
    /// Coarse classification of `delta_blocks` against the catch-up threshold.
    status: TipStatus,
}

/// Coarse status of the local node relative to the public-RPC reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TipStatus {
    /// Local within `±tolerance` blocks of the public reference (configurable
    /// via `--tip-tolerance`).
    CaughtUp,
    /// Local is more than `tolerance` blocks behind the reference.
    Behind,
    /// Local is more than `tolerance` blocks ahead of the reference.
    Ahead,
    /// Public reference fetch failed; comparison not available.
    Unavailable,
}

impl TipStatus {
    /// Display label matching the JSON serialization. Compiler-enforced
    /// exhaustive match keeps this in sync with the `serde(rename_all)`
    /// when new variants are added.
    const fn as_str(&self) -> &'static str {
        match self {
            Self::CaughtUp => "caught_up",
            Self::Behind => "behind",
            Self::Ahead => "ahead",
            Self::Unavailable => "unavailable",
        }
    }
}

impl TipReferenceJson {
    fn from_local_and_public(url: &str, local: u64, public: Option<u64>, tolerance: u64) -> Self {
        let Some(public) = public else {
            return Self {
                url: url.to_string(),
                block_number: None,
                delta_blocks: None,
                status: TipStatus::Unavailable,
            };
        };
        // delta = public - local; positive = local behind, negative = local ahead.
        // Saturating signed conversion keeps absurd RPC values from panicking;
        // real chain heights are always well under i64::MAX.
        let local_i = i64::try_from(local).unwrap_or(i64::MAX);
        let public_i = i64::try_from(public).unwrap_or(i64::MAX);
        let tolerance_i = i64::try_from(tolerance).unwrap_or(i64::MAX);
        let delta = public_i.saturating_sub(local_i);
        let status = if delta.abs() <= tolerance_i {
            TipStatus::CaughtUp
        } else if delta > 0 {
            TipStatus::Behind
        } else {
            TipStatus::Ahead
        };
        Self { url: url.to_string(), block_number: Some(public), delta_blocks: Some(delta), status }
    }
}

fn print_pretty(
    network: &str,
    report: &SyncStatusReport,
    tip_url: &str,
    public_tip_block: Option<u64>,
    tip_tolerance: u64,
) -> Result<()> {
    let cl = &report.cl;
    let mut table = KeyValueTable::new();
    table.row("network", network);

    match &report.el {
        EthSyncStatus::None => {
            table.row("el_syncing", "false");
        }
        EthSyncStatus::Info(info) => {
            let starting = info.starting_block.to::<u64>();
            let current = info.current_block.to::<u64>();
            let highest = info.highest_block.to::<u64>();
            let processed = current.saturating_sub(starting);
            let remaining = highest.saturating_sub(current);
            table.row(
                "el_syncing",
                format!(
                    "true (catching up: {remaining} blocks remaining, {processed} done; \
                     current={current} highest={highest})",
                ),
            );
        }
    }

    table
        .row("unsafe_l2", format_block_info(&cl.unsafe_l2.block_info))
        .row("safe_l2", format_block_info(&cl.safe_l2.block_info))
        .row("finalized_l2", format_block_info(&cl.finalized_l2.block_info));

    let lag_seconds =
        cl.unsafe_l2.block_info.timestamp.saturating_sub(cl.safe_l2.block_info.timestamp);
    let lag_blocks = cl.unsafe_l2.block_info.number.saturating_sub(cl.safe_l2.block_info.number);
    table.row(
        "safe_lag",
        format!(
            "{} ({} blocks behind unsafe)",
            format_duration(Duration::from_secs(lag_seconds)),
            lag_blocks,
        ),
    );

    table
        .row("l1_head", format_block_info(&cl.head_l1))
        .row("l1_safe", format_block_info(&cl.safe_l1))
        .row("l1_finalized", format_block_info(&cl.finalized_l1));

    table.row(
        "tip_reference",
        format_tip_reference(
            tip_url,
            cl.unsafe_l2.block_info.number,
            public_tip_block,
            tip_tolerance,
        ),
    );

    table.print()?;
    Ok(())
}

fn format_tip_reference(url: &str, local: u64, public: Option<u64>, tolerance: u64) -> String {
    // Single source of truth for delta math + classification:
    // `TipReferenceJson::from_local_and_public`. Build the JSON struct and
    // read its fields rather than re-deriving the same logic here.
    let tip = TipReferenceJson::from_local_and_public(url, local, public, tolerance);
    match (tip.block_number, tip.delta_blocks) {
        (Some(block), Some(delta)) => {
            format!("#{block} (url={url}) delta={delta} ({})", tip.status.as_str())
        }
        _ => format!("unavailable (url={url} fetch failed)"),
    }
}

fn format_block_info(b: &BlockInfo) -> String {
    format!("#{} ts={} ({})", b.number, b.timestamp, format_unix_timestamp(b.timestamp))
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256};
    use base_protocol::{BlockInfo, L2BlockInfo, SyncStatus};
    use basectl_cli::{MonitoringConfig, SyncStatusCommandError, SyncStatusReport};
    use url::Url;

    use super::{SyncStatusJson, resolve_cl_rpc};

    fn test_config(consensus_node_rpc: Option<Url>) -> MonitoringConfig {
        MonitoringConfig {
            name: "mainnet".to_string(),
            rpc: Url::parse("http://127.0.0.1:8545").unwrap(),
            flashblocks_ws: Url::parse("ws://127.0.0.1:7111").unwrap(),
            l1_rpc: Url::parse("http://127.0.0.1:9545").unwrap(),
            consensus_node_rpc,
            upgrades: None,
            system_config: Address::ZERO,
            batcher_address: None,
            l1_blob_target: 14,
            conductors: None,
            discovery: None,
            validators: None,
            proofs: None,
            pods: None,
        }
    }

    fn sample_l2(block: u64, ts: u64) -> L2BlockInfo {
        L2BlockInfo::new(
            BlockInfo::new(B256::repeat_byte((block & 0xff) as u8), block, B256::ZERO, ts),
            BlockNumHash { number: block / 2, hash: B256::ZERO },
            0,
        )
    }

    fn sample_l1(block: u64, ts: u64) -> BlockInfo {
        BlockInfo::new(B256::repeat_byte((block & 0xff) as u8), block, B256::ZERO, ts)
    }

    fn sample_status() -> SyncStatus {
        SyncStatus {
            current_l1: sample_l1(20_123_400, 1_780_270_000),
            current_l1_finalized: sample_l1(20_123_000, 1_780_265_000),
            head_l1: sample_l1(20_123_456, 1_780_270_500),
            safe_l1: sample_l1(20_123_400, 1_780_270_000),
            finalized_l1: sample_l1(20_123_000, 1_780_265_000),
            unsafe_l2: sample_l2(18_432_100, 1_780_274_000),
            safe_l2: sample_l2(18_431_900, 1_780_273_580),
            finalized_l2: sample_l2(18_425_000, 1_780_260_000),
            local_safe_l2: L2BlockInfo::default(),
        }
    }

    #[test]
    fn resolve_cl_rpc_errors_without_config() {
        let config = test_config(None);

        assert!(matches!(
            resolve_cl_rpc(&config, None).unwrap_err(),
            SyncStatusCommandError::MissingConsensusRpc {
                config_name,
                ..
            } if config_name == "mainnet"
        ));
    }

    #[test]
    fn sync_status_json_serializes_camelcase_with_lag_and_tip_reference() {
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        // Public reference 2 blocks ahead of local — within the caught-up
        // tolerance (5).
        let summary = SyncStatusJson::from_report(
            "mainnet",
            &report,
            "https://mainnet.base.org/",
            Some(18_432_102),
            5,
        );
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["network"], "mainnet");
        assert_eq!(value["elActivelySyncing"], false);
        assert!(value["elSyncInfo"].is_null());
        assert_eq!(value["unsafeL2"]["number"], 18_432_100);
        assert_eq!(value["safeL2"]["number"], 18_431_900);
        assert_eq!(value["finalizedL2"]["number"], 18_425_000);
        // unsafe_ts - safe_ts = 1_780_274_000 - 1_780_273_580 = 420
        assert_eq!(value["safeLagSeconds"], 420);
        assert_eq!(value["safeLagBlocks"], 200);
        assert_eq!(value["l1Head"]["number"], 20_123_456);
        assert_eq!(value["l1Safe"]["number"], 20_123_400);
        assert_eq!(value["l1Finalized"]["number"], 20_123_000);
        assert!(value["unsafeL2"]["timestamp"]["utc"].as_str().unwrap().ends_with('Z'));

        assert_eq!(value["tipReference"]["url"], "https://mainnet.base.org/");
        assert_eq!(value["tipReference"]["blockNumber"], 18_432_102);
        assert_eq!(value["tipReference"]["deltaBlocks"], 2);
        assert_eq!(value["tipReference"]["status"], "caught_up");
    }

    #[test]
    fn sync_status_json_handles_safe_ahead_of_unsafe_without_underflow() {
        // Pathological: safe head reported newer than unsafe (shouldn't happen
        // in practice, but the lag math must saturate, not panic).
        let mut status = sample_status();
        status.unsafe_l2 = sample_l2(100, 1_000);
        status.safe_l2 = sample_l2(200, 2_000);
        let report = SyncStatusReport { cl: status, el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report("mainnet", &report, "https://example/", None, 5);
        assert_eq!(summary.safe_lag_seconds, 0);
        assert_eq!(summary.safe_lag_blocks, 0);
    }

    #[test]
    fn tip_reference_classifies_behind_when_local_significantly_behind_public() {
        // Local at 18,432,100; public at 18,432,500 → 400 blocks behind.
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report(
            "mainnet",
            &report,
            "https://mainnet.base.org/",
            Some(18_432_500),
            5,
        );
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["tipReference"]["deltaBlocks"], 400);
        assert_eq!(value["tipReference"]["status"], "behind");
    }

    #[test]
    fn tip_reference_classifies_ahead_when_local_ahead_of_public() {
        // Local at 18,432,100; public at 18,431,700 → 400 ahead (negative delta).
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report(
            "mainnet",
            &report,
            "https://mainnet.base.org/",
            Some(18_431_700),
            5,
        );
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["tipReference"]["deltaBlocks"], -400);
        assert_eq!(value["tipReference"]["status"], "ahead");
    }

    #[test]
    fn tip_reference_unavailable_when_public_block_is_none() {
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        let summary =
            SyncStatusJson::from_report("mainnet", &report, "https://mainnet.base.org/", None, 5);
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert!(value["tipReference"]["blockNumber"].is_null());
        assert!(value["tipReference"]["deltaBlocks"].is_null());
        assert_eq!(value["tipReference"]["status"], "unavailable");
        assert_eq!(value["tipReference"]["url"], "https://mainnet.base.org/");
    }

    #[test]
    fn el_sync_info_includes_remaining_and_processed_when_syncing() {
        use alloy_primitives::U256;
        let info = Box::new(alloy_rpc_types_eth::SyncInfo {
            starting_block: U256::from(1_000u64),
            current_block: U256::from(1_500u64),
            highest_block: U256::from(2_000u64),
            warp_chunks_amount: None,
            warp_chunks_processed: None,
            stages: None,
        });
        let report = SyncStatusReport {
            cl: sample_status(),
            el: alloy_rpc_types_eth::SyncStatus::Info(info),
        };
        let summary = SyncStatusJson::from_report(
            "mainnet",
            &report,
            "https://example/",
            Some(18_432_100),
            5,
        );
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["elActivelySyncing"], true);
        assert_eq!(value["elSyncInfo"]["startingBlock"], 1_000);
        assert_eq!(value["elSyncInfo"]["currentBlock"], 1_500);
        assert_eq!(value["elSyncInfo"]["highestBlock"], 2_000);
        assert_eq!(value["elSyncInfo"]["processedBlocks"], 500);
        assert_eq!(value["elSyncInfo"]["remainingBlocks"], 500);
    }

    #[test]
    fn el_sync_info_saturates_when_current_exceeds_highest() {
        // Pathological: current > highest (e.g. RPC reordering during a
        // probe). remaining_blocks must saturate to 0, not underflow.
        use alloy_primitives::U256;
        let info = Box::new(alloy_rpc_types_eth::SyncInfo {
            starting_block: U256::from(1_000u64),
            current_block: U256::from(2_500u64),
            highest_block: U256::from(2_000u64),
            warp_chunks_amount: None,
            warp_chunks_processed: None,
            stages: None,
        });
        let report = SyncStatusReport {
            cl: sample_status(),
            el: alloy_rpc_types_eth::SyncStatus::Info(info),
        };
        let summary = SyncStatusJson::from_report("mainnet", &report, "https://example/", None, 5);
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["elSyncInfo"]["processedBlocks"], 1_500);
        assert_eq!(value["elSyncInfo"]["remainingBlocks"], 0);
    }

    #[test]
    fn tip_reference_tolerance_widens_caught_up_band() {
        // Same fixture as the "behind" test (delta = 400) but with tolerance
        // bumped to 500 — classification flips to caught_up, demonstrating
        // that the --tip-tolerance flag actually controls the boundary.
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report(
            "mainnet",
            &report,
            "https://mainnet.base.org/",
            Some(18_432_500),
            500,
        );
        let value: serde_json::Value = serde_json::to_value(&summary).unwrap();

        assert_eq!(value["tipReference"]["deltaBlocks"], 400);
        assert_eq!(value["tipReference"]["status"], "caught_up");
    }
}
