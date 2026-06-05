//! Implementation of the `basectl sync-status` subcommand.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_eth::SyncStatus as EthSyncStatus;
use anyhow::{Result, anyhow};
use base_protocol::{BlockInfo, L2BlockInfo};
use basectl_cli::{
    JsonOutput, KeyValueTable, MonitoringConfig, SyncStatusReport, fetch_sync_status,
    format_duration, format_unix_timestamp,
};
use chrono::{DateTime, Local, SecondsFormat};
use serde::Serialize;
use url::Url;

/// Runs the `basectl sync-status` subcommand.
pub(crate) async fn run(
    config: MonitoringConfig,
    el_rpc_override: Option<Url>,
    cl_rpc_override: Option<Url>,
    json: bool,
    raw: bool,
) -> Result<()> {
    let el_rpc = el_rpc_override.unwrap_or_else(|| config.rpc.clone());
    let cl_rpc = resolve_cl_rpc(&config, cl_rpc_override.as_ref())?;
    let report = fetch_sync_status(&el_rpc, &cl_rpc).await?;
    match (json, raw) {
        (true, true) => JsonOutput::print(&report.cl)?,
        (true, false) => {
            let summary = SyncStatusJson::from_report(&config.name, &report);
            JsonOutput::print(&summary)?;
        }
        (false, _) => print_pretty(&config.name, &report)?,
    }
    Ok(())
}

/// Resolves the consensus-node RPC URL with precedence:
/// `--cl-rpc` flag → `MonitoringConfig.consensus_node_rpc` → clear error.
///
/// The mainnet and sepolia presets ship `consensus_node_rpc: None`, so
/// non-devnet users must supply the URL explicitly.
fn resolve_cl_rpc(config: &MonitoringConfig, override_url: Option<&Url>) -> Result<Url> {
    if let Some(u) = override_url {
        return Ok(u.clone());
    }
    config.consensus_node_rpc.clone().ok_or_else(|| {
        anyhow!(
            "sync-status needs a consensus-node RPC URL.\n\
             The '{}' config does not set `consensus_node_rpc`.\n\
             Override with `--cl-rpc <url>` or set `consensus_node_rpc` in your YAML config.",
            config.name
        )
    })
}

/// Humanized JSON shape for `basectl sync-status --json`.
///
/// Decoded numerics, nested timestamp objects, and a precomputed `safeLag*`
/// pair so consumers don't have to re-derive lag from raw timestamps.
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
}

impl SyncStatusJson {
    fn from_report(network: &str, report: &SyncStatusReport) -> Self {
        let cl = &report.cl;
        let safe_lag_seconds =
            cl.unsafe_l2.block_info.timestamp.saturating_sub(cl.safe_l2.block_info.timestamp);
        let safe_lag_blocks =
            cl.unsafe_l2.block_info.number.saturating_sub(cl.safe_l2.block_info.number);
        let (el_actively_syncing, el_sync_info) = match &report.el {
            EthSyncStatus::Info(info) => (
                true,
                Some(ElSyncInfoJson {
                    starting_block: info.starting_block.to::<u64>(),
                    current_block: info.current_block.to::<u64>(),
                    highest_block: info.highest_block.to::<u64>(),
                }),
            ),
            EthSyncStatus::None => (false, None),
        };
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
}

/// Three-form timestamp object: raw unix seconds, UTC RFC 3339, and local
/// RFC 3339 (operator's machine timezone with offset suffix).
#[derive(Debug, Clone, Serialize)]
struct TimestampJson {
    unix: u64,
    utc: String,
    local: String,
}

impl TimestampJson {
    fn from_unix(secs: u64) -> Self {
        let dt = i64::try_from(secs).ok().and_then(|s| DateTime::from_timestamp(s, 0));
        let utc = dt
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_else(|| secs.to_string());
        let local = dt
            .map(|t| t.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::Secs, false))
            .unwrap_or_else(|| secs.to_string());
        Self { unix: secs, utc, local }
    }
}

fn print_pretty(network: &str, report: &SyncStatusReport) -> Result<()> {
    let cl = &report.cl;
    let mut table = KeyValueTable::new();
    table.row("network", network);

    match &report.el {
        EthSyncStatus::None => {
            table.row("el_syncing", "false");
        }
        EthSyncStatus::Info(info) => {
            table.row(
                "el_syncing",
                format!(
                    "true (current={} highest={})",
                    info.current_block.to::<u64>(),
                    info.highest_block.to::<u64>(),
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

    table.print()?;
    Ok(())
}

fn format_block_info(b: &BlockInfo) -> String {
    format!("#{} ts={} ({})", b.number, b.timestamp, format_unix_timestamp(b.timestamp))
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo, SyncStatus};
    use basectl_cli::SyncStatusReport;

    use super::{SyncStatusJson, TimestampJson};

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
    fn sync_status_json_serializes_camelcase_with_lag() {
        let report =
            SyncStatusReport { cl: sample_status(), el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report("mainnet", &report);
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
    }

    #[test]
    fn sync_status_json_handles_safe_ahead_of_unsafe_without_underflow() {
        // Pathological: safe head reported newer than unsafe (shouldn't happen
        // in practice, but the lag math must saturate, not panic).
        let mut status = sample_status();
        status.unsafe_l2 = sample_l2(100, 1_000);
        status.safe_l2 = sample_l2(200, 2_000);
        let report = SyncStatusReport { cl: status, el: alloy_rpc_types_eth::SyncStatus::None };
        let summary = SyncStatusJson::from_report("mainnet", &report);
        assert_eq!(summary.safe_lag_seconds, 0);
        assert_eq!(summary.safe_lag_blocks, 0);
    }

    #[test]
    fn timestamp_json_renders_three_forms() {
        let ts = TimestampJson::from_unix(1_780_614_804);
        assert_eq!(ts.unix, 1_780_614_804);
        assert!(ts.utc.ends_with('Z'), "expected UTC suffix Z, got {}", ts.utc);
        assert!(ts.utc.starts_with("2026-06-04"), "expected UTC date prefix, got {}", ts.utc);
    }

    #[test]
    fn timestamp_json_falls_back_on_u64_overflow() {
        let oversize = TimestampJson::from_unix(u64::MAX);
        assert_eq!(oversize.unix, u64::MAX);
        assert_eq!(oversize.utc, u64::MAX.to_string());
        assert_eq!(oversize.local, u64::MAX.to_string());
    }
}
