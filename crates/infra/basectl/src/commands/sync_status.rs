//! Implementation of the `basectl sync-status` subcommand.

use std::time::Duration;

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{BlockNumberOrTag, SyncStatus as EthSyncStatus};
use anyhow::Result;
use base_protocol::{BlockInfo, L2BlockInfo};
use clap::Args;
use serde::Serialize;
use url::Url;

use crate::{
    Format, JsonOutput, KeyValueTable, MonitoringConfig, SyncStatusReport, TimestampJson,
    fetch_block, fetch_sync_status,
};

/// Arguments for reporting combined consensus- and execution-layer sync status.
#[derive(Debug, Args)]
pub struct SyncStatusCommand {
    /// Override the execution-layer RPC URL.
    ///
    /// Defaults to the chain config's local `rpc` field.
    #[arg(long = "el-rpc", value_name = "URL")]
    pub el_rpc: Option<Url>,
    /// Override the consensus-node RPC URL.
    ///
    /// Defaults to the chain config's `consensus_node_rpc` field.
    #[arg(long = "cl-rpc", value_name = "URL")]
    pub cl_rpc: Option<Url>,
    /// Block tolerance for the tip-reference `caught_up` classification.
    ///
    /// The local node is reported as `caught_up` when within ±this many
    /// blocks of the public reference. Beyond the window, status flips
    /// to `behind` or `ahead`. Default 5 ≈ ~10s of network jitter at
    /// Base's 2s block time. Lower the value for stricter alerting,
    /// raise it to dampen noise on flaky networks.
    #[arg(long = "tip-tolerance", value_name = "BLOCKS", default_value_t = 5)]
    pub tip_tolerance: u64,
    /// Emit JSON (humanized — decoded numbers, ISO + local timestamps,
    /// precomputed `safeLag*`) instead of the pretty table.
    #[arg(long)]
    pub json: bool,
    /// With `--json`, emit the JSON-RPC wire format (the alloy-typed
    /// `optimism_syncStatus` response) instead of the humanized JSON.
    #[arg(long, requires = "json")]
    pub raw: bool,
}

impl SyncStatusCommand {
    /// Fetches sync status and renders the selected output format.
    pub async fn run(self, config: MonitoringConfig) -> Result<()> {
        let el_rpc = self.el_rpc.unwrap_or_else(|| config.rpc.clone());
        let cl_rpc = config.resolve_cl_rpc(self.cl_rpc.as_ref(), "sync-status")?;
        let public_rpc = config.public_rpc.as_ref().filter(|url| *url != &el_rpc);
        let tip_url = public_rpc.map_or(el_rpc.as_str(), Url::as_str);
        // Public tip reference is best-effort — failure marks the row unavailable
        // rather than failing the whole command. Run in parallel with the local
        // sync fetch.
        let (sync_result, tip_result) = tokio::join!(fetch_sync_status(&el_rpc, &cl_rpc), async {
            match public_rpc {
                Some(url) => fetch_block(url, BlockId::Number(BlockNumberOrTag::Latest)).await.ok(),
                None => None,
            }
        },);
        let report = sync_result?;
        let public_tip_block = tip_result.map(|block| block.header.number);

        match (self.json, self.raw) {
            (true, true) => JsonOutput::print(&report.cl)?,
            (true, false) => {
                let summary = SyncStatusJson::from_report(
                    &config.name,
                    &report,
                    tip_url,
                    public_tip_block,
                    self.tip_tolerance,
                );
                JsonOutput::print(&summary)?;
            }
            (false, _) => {
                print_pretty(
                    &config.name,
                    &report,
                    tip_url,
                    public_rpc.is_some(),
                    public_tip_block,
                    self.tip_tolerance,
                )?;
            }
        }
        Ok(())
    }
}

/// Renders sync status as the pretty key-value table.
fn print_pretty(
    network: &str,
    report: &SyncStatusReport,
    tip_url: &str,
    has_tip_reference: bool,
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
            Format::duration(Duration::from_secs(lag_seconds)),
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
            has_tip_reference,
            cl.unsafe_l2.block_info.number,
            public_tip_block,
            tip_tolerance,
        ),
    );

    table.print()?;
    Ok(())
}

/// Formats the public-tip comparison row.
fn format_tip_reference(
    url: &str,
    has_reference: bool,
    local: u64,
    public: Option<u64>,
    tolerance: u64,
) -> String {
    if !has_reference {
        return "unavailable (no independent public RPC configured)".to_string();
    }
    let tip = TipReferenceJson::from_local_and_public(url, local, public, tolerance);
    match (tip.block_number, tip.delta_blocks) {
        (Some(block), Some(delta)) => {
            format!("#{block} (url={url}) delta={delta} ({})", tip.status.as_str())
        }
        _ => format!("unavailable (url={url} fetch failed)"),
    }
}

/// Formats a block number and timestamp for pretty output.
fn format_block_info(b: &BlockInfo) -> String {
    format!("#{} ts={} ({})", b.number, b.timestamp, Format::unix_timestamp(b.timestamp))
}

/// Humanized JSON shape for `basectl sync-status --json`.
///
/// Decoded numerics, nested timestamp objects, a precomputed `safeLag*`
/// pair, and a `tipReference` block for the public-RPC comparison so
/// consumers don't have to re-derive any of these from raw fields.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusJson {
    /// Selected network name.
    pub network: String,
    /// Whether the execution layer reports active syncing.
    pub el_actively_syncing: bool,
    /// Execution-layer sync progress, when actively syncing.
    pub el_sync_info: Option<ElSyncInfoJson>,
    /// Unsafe L2 head.
    pub unsafe_l2: HeadJson,
    /// Safe L2 head.
    pub safe_l2: HeadJson,
    /// Finalized L2 head.
    pub finalized_l2: HeadJson,
    /// Timestamp lag between unsafe and safe L2 heads.
    pub safe_lag_seconds: u64,
    /// Block-number lag between unsafe and safe L2 heads.
    pub safe_lag_blocks: u64,
    /// Current L1 head.
    pub l1_head: HeadJson,
    /// Safe L1 head.
    pub l1_safe: HeadJson,
    /// Finalized L1 head.
    pub l1_finalized: HeadJson,
    /// Comparison with the public RPC tip.
    pub tip_reference: TipReferenceJson,
}

impl SyncStatusJson {
    /// Builds a humanized sync-status summary.
    pub fn from_report(
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

/// Humanized block-head reference.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadJson {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
    /// Block timestamp.
    pub timestamp: TimestampJson,
}

impl HeadJson {
    /// Builds a head from L2 block information.
    pub fn from_l2(b: &L2BlockInfo) -> Self {
        Self::from_block_info(&b.block_info)
    }

    /// Builds a head from L1 block information.
    pub fn from_l1(b: &BlockInfo) -> Self {
        Self::from_block_info(b)
    }

    /// Builds a head from shared block information.
    pub fn from_block_info(b: &BlockInfo) -> Self {
        Self { number: b.number, hash: b.hash, timestamp: TimestampJson::from_unix(b.timestamp) }
    }
}

/// Humanized execution-layer sync progress.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElSyncInfoJson {
    /// Block at which synchronization started.
    pub starting_block: u64,
    /// Current synchronized block.
    pub current_block: u64,
    /// Highest known block.
    pub highest_block: u64,
    /// Blocks processed since EL sync began (`current - starting`).
    pub processed_blocks: u64,
    /// Blocks still to process before EL sync completes (`highest - current`).
    pub remaining_blocks: u64,
}

/// Comparison of the local node's unsafe L2 head against an independent public
/// RPC reference.
/// Best-effort — when the fetch fails, `block_number` and `delta_blocks` are
/// `None` and `status` is `unavailable`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TipReferenceJson {
    /// The effective public-reference RPC URL queried.
    pub url: String,
    /// Latest block number reported by the public RPC. `None` if the call failed.
    pub block_number: Option<u64>,
    /// Signed delta `public - local`. Positive means local is behind; negative
    /// means local is ahead. `None` if the public block isn't known.
    pub delta_blocks: Option<i64>,
    /// Coarse classification of `delta_blocks` against the catch-up threshold.
    pub status: TipStatus,
}

/// Coarse status of the local node relative to the public-RPC reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TipStatus {
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
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::CaughtUp => "caught_up",
            Self::Behind => "behind",
            Self::Ahead => "ahead",
            Self::Unavailable => "unavailable",
        }
    }
}

impl TipReferenceJson {
    /// Compares a local block number with an optional public reference.
    pub fn from_local_and_public(
        url: &str,
        local: u64,
        public: Option<u64>,
        tolerance: u64,
    ) -> Self {
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

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, U256};
    use base_protocol::{BlockInfo, L2BlockInfo, SyncStatus};

    use super::{SyncStatusJson, format_tip_reference};
    use crate::SyncStatusReport;

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
    fn tip_reference_is_unavailable_without_independent_public_rpc() {
        assert_eq!(
            format_tip_reference("http://127.0.0.1:8545/", false, 100, Some(100), 5),
            "unavailable (no independent public RPC configured)"
        );
    }

    #[test]
    fn el_sync_info_includes_remaining_and_processed_when_syncing() {
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
