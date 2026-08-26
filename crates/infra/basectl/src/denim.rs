//! Reusable Denim activation checks over one hash-pinned L2 snapshot.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::{
    Network, Provider, ProviderBuilder,
    network::{ReceiptResponse, TransactionResponse},
};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionRequest};
use alloy_sol_types::SolCall;
use alloy_transport_http::Http;
use anyhow::{Context, Result, anyhow};
use base_common_consensus::Predeploys;
use base_common_evm::BaseTime;
use base_common_network::Base;
use base_consensus_rpc::RollupNodeApiClient;
use base_protocol::BaseTimeUpdateTx;
use futures::{SinkExt, StreamExt};
use jsonrpsee::http_client::HttpClientBuilder;
use serde::Serialize;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

alloy_sol_types::sol! {
    function timestampMillisPart() external view returns (uint16);
    function timestampMs() external view returns (uint64);
}

/// Snapshot selection for a Denim check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenimCheckTarget {
    /// Resolve the latest block once, then pin all reads to its hash.
    Latest,
    /// Check the snapshot identified by this block hash.
    BlockHash(B256),
}

/// Last hash-pinned snapshot checked by a caller that polls `Latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenimCheckCursor {
    /// Checked block number.
    pub block_number: u64,
    /// Checked block hash.
    pub block_hash: B256,
}

/// Denim schedule state at the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DenimSchedule {
    /// The CL has no Denim activation configured.
    NotScheduled,
    /// Denim is configured after the snapshot timestamp.
    Scheduled,
    /// Denim is active at the snapshot timestamp.
    Active,
}

/// Status of a report dimension or individual check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DenimCheckStatus {
    /// The invariant holds.
    Pass,
    /// The invariant does not hold.
    Fail,
    /// The invariant does not apply or cannot be established yet.
    Indeterminate,
}

/// One evaluated Denim invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DenimCheck {
    /// Stable check name.
    pub name: String,
    /// Check result.
    pub status: DenimCheckStatus,
    /// Expected value.
    pub expected: String,
    /// Observed value.
    pub observed: String,
}

/// Serializable report for one hash-pinned Denim snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DenimReport {
    /// Activation state.
    pub schedule: DenimSchedule,
    /// Explicit snapshot block number.
    pub block_number: u64,
    /// Explicit snapshot block hash.
    pub block_hash: B256,
    /// Snapshot timestamp in seconds.
    pub timestamp: u64,
    /// Snapshot timestamp in milliseconds, when exposed.
    pub timestamp_ms: Option<u64>,
    /// CL Denim activation timestamp.
    pub activation: Option<u64>,
    /// Detailed checks.
    pub checks: Vec<DenimCheck>,
    /// Safe polling cursor, absent when cadence ancestry was incomplete.
    #[serde(skip)]
    pub cursor: Option<DenimCheckCursor>,
}

/// Pure observations used to evaluate a Denim report.
#[derive(Debug, Clone)]
pub struct DenimObservations {
    /// CL Denim activation timestamp.
    pub activation: Option<u64>,
    /// Snapshot block number.
    pub block_number: u64,
    /// Snapshot block hash.
    pub block_hash: B256,
    /// Snapshot timestamp in seconds.
    pub timestamp: u64,
    /// Header timestamp in milliseconds.
    pub timestamp_ms: Option<u64>,
    /// Proxy code hash.
    pub proxy_code_hash: B256,
    /// Proxy admin slot.
    pub admin: U256,
    /// Proxy implementation slot.
    pub implementation: U256,
    /// Whether the implementation slot is a canonical encoded address.
    pub implementation_is_address: bool,
    /// Linked implementation code hash, if any.
    pub implementation_code_hash: Option<B256>,
    /// Packed storage timestamp millisecond part.
    pub storage_millis_part: u16,
    /// Canonical metadata millisecond part, if valid.
    pub metadata_millis_part: Option<u16>,
    /// Metadata validation error, if invalid.
    pub metadata_error: Option<String>,
    /// Whether the pinned metadata receipt is canonical and successful.
    pub metadata_receipt_valid: Option<bool>,
    /// `timestampMillisPart()` result.
    pub getter_millis_part: Option<u16>,
    /// `timestampMillisPart()` call or decode error.
    pub getter_millis_part_error: Option<String>,
    /// `timestampMs()` result.
    pub getter_timestamp_ms: Option<u64>,
    /// `timestampMs()` call or decode error.
    pub getter_timestamp_ms_error: Option<String>,
}

/// Denim RPC entry point.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenimChecker;

impl DenimChecker {
    /// Checks a snapshot, including every cadence edge after `previous` for `Latest`.
    pub async fn check(
        &self,
        el_rpc: &Url,
        cl_rpc: &Url,
        el_ws_rpc: Option<&Url>,
        target: DenimCheckTarget,
        previous: Option<DenimCheckCursor>,
    ) -> Result<DenimReport> {
        let http = alloy_transport_http::reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_client(RpcClient::new(Http::with_client(http, el_rpc.clone()), false));
        let cl = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(10))
            .build(cl_rpc.as_str())
            .with_context(|| format!("connecting to consensus node RPC at {cl_rpc}"))?;
        let rollup = RollupNodeApiClient::rollup_config(&cl)
            .await
            .with_context(|| format!("fetching optimism_rollupConfig from {cl_rpc}"))?;
        let hash = match target {
            DenimCheckTarget::Latest => {
                provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .full()
                    .await?
                    .ok_or_else(|| anyhow!("latest block not found"))?
                    .header
                    .hash
            }
            DenimCheckTarget::BlockHash(hash) => hash,
        };
        let block_id = BlockId::Hash(hash.into());
        let full_hash = provider
            .get_block(block_id)
            .full()
            .await?
            .ok_or_else(|| anyhow!("snapshot block by hash not found"))?;
        if full_hash.header.hash != hash {
            return Err(anyhow!("snapshot block response did not match the requested hash"));
        }
        let number = full_hash.header.number;

        let (proxy_code, admin, implementation, storage) = tokio::try_join!(
            provider.get_code_at(Predeploys::BASE_TIME).block_id(block_id),
            provider.get_storage_at(Predeploys::BASE_TIME, BaseTime::ADMIN_SLOT).block_id(block_id),
            provider
                .get_storage_at(Predeploys::BASE_TIME, BaseTime::IMPLEMENTATION_SLOT)
                .block_id(block_id),
            provider
                .get_storage_at(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT)
                .block_id(block_id),
        )?;
        let implementation_bytes = implementation.to_be_bytes::<32>();
        let implementation_is_address = implementation_bytes[..12].iter().all(|byte| *byte == 0);
        let implementation_address = Address::from_slice(&implementation_bytes[12..]);
        let implementation_code_hash = if implementation != U256::ZERO && implementation_is_address
        {
            let code = provider
                .get_code_at(implementation_address)
                .block_id(block_id)
                .await
                .context("fetching BaseTime implementation code at snapshot block")?;
            Some(keccak256(code))
        } else {
            None
        };

        let active = rollup.upgrades.base.denim.is_some_and(|at| full_hash.header.timestamp >= at);
        let envelopes: Vec<_> =
            full_hash.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect();
        let metadata = active
            .then(|| BaseTimeUpdateTx::extract_from_transactions(&envelopes, number))
            .transpose();
        let (metadata_millis_part, metadata_error) = match metadata {
            Ok(Some(value)) => (Some(value.timestamp_millis_part()), None),
            Err(error) => (None, Some(error.to_string())),
            Ok(None) => (None, None),
        };
        let metadata_receipt_valid = if active {
            let tx = full_hash.transactions.txns().nth(1);
            let receipts = provider
                .get_block_receipts(block_id)
                .await
                .context("fetching hash-pinned BaseTime metadata receipt")?;
            match (tx, receipts.and_then(|receipts| receipts.into_iter().nth(1))) {
                (Some(tx), Some(receipt)) => {
                    let tx_hash = tx.tx_hash();
                    Some(
                        receipt.status()
                            && receipt.transaction_hash() == tx_hash
                            && receipt.block_hash() == Some(hash)
                            && receipt.block_number() == Some(number)
                            && receipt.transaction_index() == Some(1),
                    )
                }
                _ => Some(false),
            }
        } else {
            None
        };
        let call = |data| TransactionRequest::default().to(Predeploys::BASE_TIME).input(data);
        let (getter_millis_part, getter_millis_part_error) = if active {
            match provider
                .call(call(timestampMillisPartCall {}.abi_encode().into()).into())
                .block(block_id)
                .await
            {
                Ok(bytes) => match timestampMillisPartCall::abi_decode_returns(&bytes) {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error.to_string())),
                },
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let (getter_timestamp_ms, getter_timestamp_ms_error) = if active {
            match provider
                .call(call(timestampMsCall {}.abi_encode().into()).into())
                .block(block_id)
                .await
            {
                Ok(bytes) => match timestampMsCall::abi_decode_returns(&bytes) {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error.to_string())),
                },
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };

        let mut report = DenimReport::evaluate(DenimObservations {
            activation: rollup.upgrades.base.denim,
            block_number: number,
            block_hash: hash,
            timestamp: full_hash.header.timestamp,
            timestamp_ms: full_hash.header.timestamp_ms,
            proxy_code_hash: keccak256(proxy_code),
            admin,
            implementation,
            implementation_is_address,
            implementation_code_hash,
            storage_millis_part: BaseTime::decode_timestamp_millis_part(storage),
            metadata_millis_part,
            metadata_error,
            metadata_receipt_valid,
            getter_millis_part,
            getter_millis_part_error,
            getter_timestamp_ms,
            getter_timestamp_ms_error,
        });
        let cadence_complete = append_cadence_check(&provider, &mut report, target, previous).await;
        let cadence_passed = report
            .checks
            .iter()
            .find(|check| check.name == "cadence_200ms")
            .is_some_and(|check| check.status == DenimCheckStatus::Pass);
        report.cursor = (cadence_complete && cadence_passed).then_some(DenimCheckCursor {
            block_number: report.block_number,
            block_hash: report.block_hash,
        });
        append_wire_checks(&provider, &mut report, &full_hash, el_ws_rpc, target).await;
        Ok(report)
    }
}

#[derive(Debug)]
enum RawOutcome {
    Value(Value),
    MethodError(String),
    Unavailable(String),
}

async fn raw_call<P: Provider<Base>>(
    provider: &P,
    method: &'static str,
    params: Value,
) -> RawOutcome {
    match provider.raw_request::<Value, Value>(method.into(), params).await {
        Ok(value) => RawOutcome::Value(value),
        Err(error) => {
            if let Some(response) = error.as_error_resp() {
                return RawOutcome::MethodError(response.message.to_string());
            }
            if let Some(body) = error
                .as_transport_err()
                .and_then(|error| error.as_http_error())
                .and_then(|error| serde_json::from_str::<Value>(&error.body).ok())
            {
                if let Some(message) = body.pointer("/error/message").and_then(Value::as_str) {
                    return RawOutcome::MethodError(message.into());
                }
                if let Some(result) = body.get("result") {
                    return RawOutcome::Value(result.clone());
                }
            }
            RawOutcome::Unavailable(error.to_string())
        }
    }
}

fn check_wire_object(
    report: &mut DenimReport,
    name: &str,
    outcome: RawOutcome,
    field: &str,
    expected_timestamp: u64,
    identity: &[(&str, String)],
) {
    match outcome {
        RawOutcome::Value(value) if value.is_null() => {
            report.add_check(name, DenimCheckStatus::Fail, field, "known snapshot object missing")
        }
        RawOutcome::Value(value) => {
            let object = value.as_object();
            let timestamp = object
                .and_then(|object| object.get(field))
                .and_then(Value::as_str)
                .and_then(parse_quantity);
            let identity_matches = identity.iter().all(|(field, expected)| {
                let actual = object.and_then(|object| object.get(*field)).and_then(Value::as_str);
                if field.to_ascii_lowercase().contains("hash") {
                    expected.parse::<B256>().ok().is_some_and(|expected| {
                        actual.and_then(|value| value.parse::<B256>().ok()) == Some(expected)
                    })
                } else {
                    parse_quantity(expected)
                        .is_some_and(|expected| actual.and_then(parse_quantity) == Some(expected))
                }
            });
            let pass = timestamp == Some(expected_timestamp) && identity_matches;
            report.add_check(
                name,
                if pass { DenimCheckStatus::Pass } else { DenimCheckStatus::Fail },
                format!("{field}=0x{expected_timestamp:x} with pinned identity"),
                timestamp.map_or_else(
                    || format!("missing or invalid {field}"),
                    |v| {
                        if identity_matches {
                            format!("0x{v:x}")
                        } else {
                            "identity mismatch".into()
                        }
                    },
                ),
            );
        }
        RawOutcome::MethodError(error) => {
            report.add_check(name, DenimCheckStatus::Fail, field, error)
        }
        RawOutcome::Unavailable(error) => {
            report.add_check(name, DenimCheckStatus::Indeterminate, field, error)
        }
    }
}

fn check_wire_logs(
    report: &mut DenimReport,
    name: &str,
    outcome: RawOutcome,
    expected_timestamp: u64,
    block_hash: &str,
) {
    match outcome {
        RawOutcome::Value(value) => {
            let Some(logs) = value.as_array() else {
                report.add_check(name, DenimCheckStatus::Fail, "log array", "invalid result");
                return;
            };
            if logs.is_empty() {
                report.add_check(
                    name,
                    DenimCheckStatus::Fail,
                    "at least one pinned log",
                    "no logs in evidence block",
                );
                return;
            }
            let valid = logs.iter().all(|log| {
                block_hash.parse::<B256>().ok().is_some_and(|expected| {
                    log.get("blockHash")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<B256>().ok())
                        == Some(expected)
                }) && log.get("blockTimestampMs").and_then(Value::as_str).and_then(parse_quantity)
                    == Some(expected_timestamp)
            });
            report.add_check(
                name,
                if valid { DenimCheckStatus::Pass } else { DenimCheckStatus::Fail },
                "blockTimestampMs with pinned blockHash",
                if valid { "conformant" } else { "missing, wrong, or mismatched log field" },
            );
        }
        RawOutcome::MethodError(error) => {
            report.add_check(name, DenimCheckStatus::Fail, "blockTimestampMs", error)
        }
        RawOutcome::Unavailable(error) => {
            report.add_check(name, DenimCheckStatus::Indeterminate, "blockTimestampMs", error)
        }
    }
}

fn parse_quantity(value: &str) -> Option<u64> {
    let digits = value.strip_prefix("0x")?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn cadence_status(parent: Option<u64>, child: Option<u64>, contiguous: bool) -> DenimCheckStatus {
    match (parent, child, contiguous) {
        (Some(parent), Some(child), true) if parent.checked_add(200) == Some(child) => {
            DenimCheckStatus::Pass
        }
        (Some(_), Some(_), true) => DenimCheckStatus::Fail,
        _ => DenimCheckStatus::Indeterminate,
    }
}

async fn append_cadence_check<P: Provider<Base>>(
    provider: &P,
    report: &mut DenimReport,
    target: DenimCheckTarget,
    previous: Option<DenimCheckCursor>,
) -> bool {
    if previous.is_some_and(|cursor| {
        matches!(target, DenimCheckTarget::Latest)
            && cursor.block_number == report.block_number
            && cursor.block_hash == report.block_hash
    }) {
        report.add_check(
            "cadence_200ms",
            DenimCheckStatus::Pass,
            "every canonical parent-child edge is exactly +200ms",
            "no new blocks",
        );
        return true;
    }
    let stop = match (target, previous) {
        (DenimCheckTarget::Latest, Some(cursor)) => Some(cursor),
        _ => None,
    };
    let mut hash = report.block_hash;
    let mut child_ms = None;
    let mut status = DenimCheckStatus::Pass;
    let mut complete = false;
    let mut checked_edge = false;
    let mut child_number = None;
    let mut replacement_cursor = false;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(hash) {
            status = DenimCheckStatus::Indeterminate;
            break;
        }
        let block = match provider.get_block(BlockId::Hash(hash.into())).full().await {
            Ok(Some(block)) => block,
            _ => {
                status = DenimCheckStatus::Indeterminate;
                break;
            }
        };
        if block.header.hash != hash {
            status = DenimCheckStatus::Indeterminate;
            break;
        }
        let envelopes: Vec<_> =
            block.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect();
        let millis = BaseTimeUpdateTx::extract_from_transactions(&envelopes, block.header.number)
            .ok()
            .map(|metadata| {
                block.header.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part())
            });
        let active = report.activation.is_some_and(|at| block.header.timestamp >= at);
        if active && millis.is_none() {
            status = DenimCheckStatus::Fail;
        }
        let mut edge_checked = false;
        if active && child_ms.is_some() {
            if child_number != block.header.number.checked_add(1) {
                status = DenimCheckStatus::Indeterminate;
                break;
            }
            edge_checked = true;
            checked_edge = true;
            let edge = cadence_status(millis, child_ms, true);
            if edge != DenimCheckStatus::Pass && status != DenimCheckStatus::Fail {
                status = edge;
            }
        }
        if stop.is_some_and(|cursor| {
            cursor.block_number == block.header.number && cursor.block_hash == block.header.hash
        }) {
            complete = true;
            break;
        }
        if replacement_cursor && edge_checked {
            complete = true;
            break;
        }
        if stop.is_some_and(|cursor| block.header.number == cursor.block_number) {
            replacement_cursor = true;
        } else if stop.is_some_and(|cursor| block.header.number < cursor.block_number) {
            status = DenimCheckStatus::Indeterminate;
            break;
        }
        if stop.is_none() && checked_edge {
            complete = true;
            break;
        }
        if !active {
            if stop.is_some() {
                status = DenimCheckStatus::Indeterminate;
            } else {
                complete = true;
            }
            break;
        }
        child_ms = millis;
        child_number = Some(block.header.number);
        hash = block.header.parent_hash;
    }
    if status == DenimCheckStatus::Pass && !checked_edge {
        status = DenimCheckStatus::Indeterminate;
    }
    report.add_check(
        "cadence_200ms",
        status,
        "every canonical parent-child edge is exactly +200ms",
        match status {
            DenimCheckStatus::Pass => "exact +200ms progression",
            DenimCheckStatus::Fail => "wrong timestamp gap",
            DenimCheckStatus::Indeterminate => "activation boundary, missing range, or reorg",
        },
    );
    complete
}

async fn append_wire_checks<P: Provider<Base>>(
    provider: &P,
    report: &mut DenimReport,
    block: &<Base as Network>::BlockResponse,
    el_ws_rpc: Option<&Url>,
    target: DenimCheckTarget,
) {
    let transactions =
        block.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect::<Vec<_>>();
    let timestamp_ms =
        BaseTimeUpdateTx::extract_from_transactions(&transactions, report.block_number)
            .ok()
            .map(|metadata| report.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part()));
    if let Some(timestamp_ms) = timestamp_ms {
        let hash = report.block_hash.to_string();
        let number = format!("0x{:x}", report.block_number);
        for (name, method, params) in [
            ("rpc_eth_getBlockByHash_timestampMs", "eth_getBlockByHash", json!([hash, false])),
            (
                "rpc_eth_getBlockByNumber_timestampMs",
                "eth_getBlockByNumber",
                json!([number, false]),
            ),
            ("rpc_eth_getHeaderByHash_timestampMs", "eth_getHeaderByHash", json!([hash])),
            ("rpc_eth_getHeaderByNumber_timestampMs", "eth_getHeaderByNumber", json!([number])),
        ] {
            let identity = [("hash", hash.clone()), ("number", number.clone())];
            check_wire_object(
                report,
                name,
                raw_call(provider, method, params).await,
                "timestampMs",
                timestamp_ms,
                &identity,
            );
        }
        if let Some(tx) = block.transactions.txns().nth(1) {
            let tx_hash = tx.tx_hash().to_string();
            let tx_index = "0x1".to_string();
            for (name, method, params) in [
                (
                    "rpc_eth_getTransactionByHash_blockTimestampMs",
                    "eth_getTransactionByHash",
                    json!([tx_hash]),
                ),
                (
                    "rpc_eth_getTransactionByBlockHashAndIndex_blockTimestampMs",
                    "eth_getTransactionByBlockHashAndIndex",
                    json!([hash, tx_index]),
                ),
                (
                    "rpc_eth_getTransactionByBlockNumberAndIndex_blockTimestampMs",
                    "eth_getTransactionByBlockNumberAndIndex",
                    json!([number, tx_index]),
                ),
            ] {
                let identity = [
                    ("hash", tx_hash.clone()),
                    ("blockHash", hash.clone()),
                    ("blockNumber", number.clone()),
                    ("transactionIndex", tx_index.clone()),
                ];
                check_wire_object(
                    report,
                    name,
                    raw_call(provider, method, params).await,
                    "blockTimestampMs",
                    timestamp_ms,
                    &identity,
                );
            }
        }
    } else {
        for name in [
            "rpc_eth_getBlockByHash_timestampMs",
            "rpc_eth_getBlockByNumber_timestampMs",
            "rpc_eth_getHeaderByHash_timestampMs",
            "rpc_eth_getHeaderByNumber_timestampMs",
            "rpc_eth_getTransactionByHash_blockTimestampMs",
            "rpc_eth_getTransactionByBlockHashAndIndex_blockTimestampMs",
            "rpc_eth_getTransactionByBlockNumberAndIndex_blockTimestampMs",
        ] {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "canonical metadata timestamp",
                "canonical BaseTime metadata unavailable",
            );
        }
    }

    const LOG_CHECKS: [&str; 5] = [
        "rpc_eth_getLogs_blockTimestampMs",
        "rpc_eth_getFilterChanges_blockTimestampMs",
        "rpc_eth_getFilterLogs_blockTimestampMs",
        "rpc_eth_getTransactionReceipt_logs_blockTimestampMs",
        "rpc_eth_getBlockReceipts_logs_blockTimestampMs",
    ];
    const LOG_LOOKBACK_BLOCKS: u64 = 200_000;
    const LOG_DISCOVERY_CALLS: usize = 16;
    const INITIAL_LOG_WINDOW: u64 = 32;
    const MAX_LOG_WINDOW: u64 = 32_768;
    let add_indeterminate_log_checks = |report: &mut DenimReport, reason: &str| {
        for name in LOG_CHECKS {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "post-Denim log evidence",
                reason,
            );
        }
    };
    if report.schedule != DenimSchedule::Active {
        add_indeterminate_log_checks(report, "Denim is not active at the snapshot");
        append_subscription_checks(provider, report, el_ws_rpc, target).await;
        return;
    }

    let floor = report.block_number.saturating_sub(LOG_LOOKBACK_BLOCKS);
    let mut upper = report.block_number;
    let mut window = INITIAL_LOG_WINDOW;
    let mut max_window = MAX_LOG_WINDOW;
    let mut evidence = None;
    let mut discovery_reason = "no post-Denim log found within scan bounds".to_string();
    for _ in 0..LOG_DISCOVERY_CALLS {
        let width = window.min(upper - floor + 1);
        let lower = upper - (width - 1);
        let filter = json!({
            "fromBlock": format!("0x{lower:x}"),
            "toBlock": format!("0x{upper:x}"),
        });
        match raw_call(provider, "eth_getLogs", json!([filter])).await {
            RawOutcome::Value(value) => {
                let Some(logs) = value.as_array() else {
                    discovery_reason = "eth_getLogs returned an invalid log array".into();
                    break;
                };
                evidence = logs
                    .iter()
                    .filter_map(|log| {
                        let block_number =
                            log.get("blockNumber")?.as_str().and_then(parse_quantity)?;
                        let transaction_index =
                            log.get("transactionIndex")?.as_str().and_then(parse_quantity)?;
                        let block_hash = log.get("blockHash")?.as_str()?.parse::<B256>().ok()?;
                        let transaction_hash =
                            log.get("transactionHash")?.as_str()?.parse::<B256>().ok()?;
                        (block_number >= lower && block_number <= upper).then_some((
                            block_number,
                            transaction_index,
                            block_hash,
                            transaction_hash,
                        ))
                    })
                    .max_by_key(|(block_number, transaction_index, _, _)| {
                        (*block_number, *transaction_index)
                    });
                if evidence.is_some() {
                    break;
                }
                if !logs.is_empty() {
                    discovery_reason = "eth_getLogs returned malformed log identity".into();
                    break;
                }
                discovery_reason = "no post-Denim log found within scan bounds".into();
                if lower == floor {
                    break;
                }
                upper = lower - 1;
                window = window.saturating_mul(4).min(max_window);
            }
            RawOutcome::MethodError(error) if width > 1 => {
                discovery_reason = error;
                max_window = (width / 4).max(1);
                window = max_window;
            }
            RawOutcome::MethodError(error) | RawOutcome::Unavailable(error) => {
                discovery_reason = error;
                break;
            }
        }
    }
    let Some((evidence_number, evidence_tx_index, evidence_hash, evidence_tx_hash)) = evidence
    else {
        add_indeterminate_log_checks(report, &discovery_reason);
        append_subscription_checks(provider, report, el_ws_rpc, target).await;
        return;
    };
    let evidence_block = match provider.get_block(BlockId::Hash(evidence_hash.into())).full().await
    {
        Ok(Some(block)) => block,
        Ok(None) => {
            add_indeterminate_log_checks(report, "log evidence block unavailable");
            append_subscription_checks(provider, report, el_ws_rpc, target).await;
            return;
        }
        Err(error) => {
            add_indeterminate_log_checks(report, &error.to_string());
            append_subscription_checks(provider, report, el_ws_rpc, target).await;
            return;
        }
    };
    let evidence_active =
        report.activation.is_some_and(|activation| evidence_block.header.timestamp >= activation);
    let evidence_identity_matches = evidence_block.header.hash == evidence_hash
        && evidence_block.header.number == evidence_number
        && usize::try_from(evidence_tx_index)
            .ok()
            .and_then(|index| evidence_block.transactions.txns().nth(index))
            .is_some_and(|transaction| transaction.tx_hash() == evidence_tx_hash);
    let evidence_transactions = evidence_block
        .transactions
        .txns()
        .take(2)
        .map(|tx| tx.as_ref().clone())
        .collect::<Vec<_>>();
    let evidence_timestamp_ms = BaseTimeUpdateTx::extract_from_transactions(
        &evidence_transactions,
        evidence_block.header.number,
    )
    .ok()
    .map(|metadata| {
        evidence_block.header.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part())
    });
    let Some(evidence_timestamp_ms) =
        (evidence_active && evidence_identity_matches).then_some(evidence_timestamp_ms).flatten()
    else {
        let reason = if !evidence_active {
            "newest log predates Denim activation"
        } else if !evidence_identity_matches {
            "log evidence no longer matches its canonical transaction"
        } else {
            "log evidence block has invalid BaseTime metadata"
        };
        add_indeterminate_log_checks(report, reason);
        append_subscription_checks(provider, report, el_ws_rpc, target).await;
        return;
    };

    let hash = evidence_hash.to_string();
    let tx_hash = evidence_tx_hash.to_string();
    let filter = json!({"blockHash": hash});
    check_wire_logs(
        report,
        "rpc_eth_getLogs_blockTimestampMs",
        raw_call(provider, "eth_getLogs", json!([filter])).await,
        evidence_timestamp_ms,
        &hash,
    );
    let filter_id = raw_call(provider, "eth_newFilter", json!([filter])).await;
    match filter_id {
        RawOutcome::Value(id) if id.as_str().is_some_and(|id| !id.is_empty()) => {
            check_wire_logs(
                report,
                "rpc_eth_getFilterChanges_blockTimestampMs",
                raw_call(provider, "eth_getFilterChanges", json!([id])).await,
                evidence_timestamp_ms,
                &hash,
            );
            check_wire_logs(
                report,
                "rpc_eth_getFilterLogs_blockTimestampMs",
                raw_call(provider, "eth_getFilterLogs", json!([id])).await,
                evidence_timestamp_ms,
                &hash,
            );
            let cleanup = raw_call(provider, "eth_uninstallFilter", json!([id])).await;
            if !matches!(cleanup, RawOutcome::Value(Value::Bool(true))) {
                for name in [
                    "rpc_eth_getFilterChanges_blockTimestampMs",
                    "rpc_eth_getFilterLogs_blockTimestampMs",
                ] {
                    if let Some(check) =
                        report.checks.iter_mut().rev().find(|check| check.name == name)
                    {
                        check.status = DenimCheckStatus::Fail;
                        check.observed = "filter cleanup failed".into();
                    }
                }
            }
        }
        outcome => {
            for name in [
                "rpc_eth_getFilterChanges_blockTimestampMs",
                "rpc_eth_getFilterLogs_blockTimestampMs",
            ] {
                let (status, reason) = match &outcome {
                    RawOutcome::MethodError(error) => (DenimCheckStatus::Fail, error.as_str()),
                    RawOutcome::Unavailable(error) => {
                        (DenimCheckStatus::Indeterminate, error.as_str())
                    }
                    RawOutcome::Value(_) => (DenimCheckStatus::Fail, "invalid filter identifier"),
                };
                report.add_check(name, status, "blockHash filter", reason);
            }
        }
    }
    let receipt = raw_call(provider, "eth_getTransactionReceipt", json!([tx_hash])).await;
    let receipt_logs = match receipt {
        RawOutcome::Value(value)
            if value.get("transactionHash").and_then(Value::as_str) == Some(tx_hash.as_str())
                && value.get("blockHash").and_then(Value::as_str) == Some(hash.as_str())
                && value.get("blockNumber").and_then(Value::as_str).and_then(parse_quantity)
                    == Some(evidence_number) =>
        {
            RawOutcome::Value(value.get("logs").cloned().unwrap_or(Value::Null))
        }
        RawOutcome::Value(_) => RawOutcome::Value(Value::Null),
        other => other,
    };
    check_wire_logs(
        report,
        "rpc_eth_getTransactionReceipt_logs_blockTimestampMs",
        receipt_logs,
        evidence_timestamp_ms,
        &hash,
    );
    let receipts = raw_call(provider, "eth_getBlockReceipts", json!([hash])).await;
    let nested = match receipts {
        RawOutcome::Value(value) => match value.as_array() {
            Some(receipts)
                if receipts.iter().all(|receipt| {
                    receipt.get("blockHash").and_then(Value::as_str) == Some(hash.as_str())
                        && receipt
                            .get("blockNumber")
                            .and_then(Value::as_str)
                            .and_then(parse_quantity)
                            == Some(evidence_number)
                        && receipt.get("logs").is_some_and(Value::is_array)
                }) =>
            {
                RawOutcome::Value(Value::Array(
                    receipts
                        .iter()
                        .flat_map(|receipt| {
                            receipt.get("logs").and_then(Value::as_array).unwrap().iter().cloned()
                        })
                        .collect(),
                ))
            }
            _ => RawOutcome::Value(Value::Null),
        },
        other => other,
    };
    check_wire_logs(
        report,
        "rpc_eth_getBlockReceipts_logs_blockTimestampMs",
        nested,
        evidence_timestamp_ms,
        &hash,
    );
    append_subscription_checks(provider, report, el_ws_rpc, target).await;
}

async fn subscription_event_result<P: Provider<Base>>(
    provider: &P,
    kind: &str,
    notification: &Value,
) -> (DenimCheckStatus, String) {
    let Some(result) = notification.pointer("/params/result") else {
        return (DenimCheckStatus::Fail, "notification has no result".into());
    };
    let block_hash = match kind {
        "newHeads" => result.get("hash").and_then(Value::as_str),
        "logs" => result.get("blockHash").and_then(Value::as_str),
        "transactionReceipts" => result
            .as_array()
            .and_then(|receipts| receipts.first())
            .or(Some(result))
            .and_then(|receipt| receipt.get("blockHash"))
            .and_then(Value::as_str),
        _ => None,
    };
    let Some(hash) = block_hash.and_then(|hash| hash.parse::<B256>().ok()) else {
        return (DenimCheckStatus::Fail, "event has no valid block hash".into());
    };
    let block = match provider.get_block(BlockId::Hash(hash.into())).full().await {
        Ok(Some(block)) if block.header.hash == hash => block,
        _ => return (DenimCheckStatus::Indeterminate, "event block unavailable".into()),
    };
    let transactions =
        block.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect::<Vec<_>>();
    let Some(expected) =
        BaseTimeUpdateTx::extract_from_transactions(&transactions, block.header.number).ok().map(
            |metadata| block.header.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part()),
        )
    else {
        return (DenimCheckStatus::Indeterminate, "event block metadata unavailable".into());
    };
    if kind == "transactionReceipts" {
        let Some(receipts) = result.as_array().filter(|receipts| !receipts.is_empty()) else {
            return (DenimCheckStatus::Fail, "malformed receipt event".into());
        };
        if !receipts.iter().all(|receipt| {
            receipt
                .get("blockHash")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<B256>().ok())
                == Some(hash)
                && receipt.get("blockNumber").and_then(Value::as_str).and_then(parse_quantity)
                    == Some(block.header.number)
                && receipt.get("logs").is_some_and(Value::is_array)
        }) {
            return (DenimCheckStatus::Fail, "malformed or mismatched receipt event".into());
        }
        let logs: Vec<_> = receipts
            .iter()
            .filter_map(|receipt| receipt.get("logs").and_then(Value::as_array))
            .flatten()
            .collect();
        if logs.is_empty() {
            return (DenimCheckStatus::Indeterminate, "receipt event has no logs".into());
        }
        let valid = logs.iter().all(|log| {
            log.get("blockHash")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<B256>().ok())
                == Some(hash)
                && log.get("blockTimestampMs").and_then(Value::as_str).and_then(parse_quantity)
                    == Some(expected)
        });
        return (
            if valid { DenimCheckStatus::Pass } else { DenimCheckStatus::Fail },
            if valid {
                format!("matching event timestamp 0x{expected:x}")
            } else {
                "missing or incorrect event timestamp".into()
            },
        );
    }
    let valid = match kind {
        "newHeads" => {
            result.get("timestampMs").and_then(Value::as_str).and_then(parse_quantity)
                == Some(expected)
        }
        "logs" => {
            result.get("blockTimestampMs").and_then(Value::as_str).and_then(parse_quantity)
                == Some(expected)
        }
        _ => false,
    };
    (
        if valid { DenimCheckStatus::Pass } else { DenimCheckStatus::Fail },
        if valid {
            format!("matching event timestamp 0x{expected:x}")
        } else {
            "missing or incorrect event timestamp".into()
        },
    )
}

async fn append_subscription_checks<P: Provider<Base>>(
    provider: &P,
    report: &mut DenimReport,
    el_ws_rpc: Option<&Url>,
    target: DenimCheckTarget,
) {
    const SUBSCRIPTIONS: [(&str, &str); 3] = [
        ("rpc_eth_subscribe_newHeads_timestampMs", "newHeads"),
        ("rpc_eth_subscribe_logs_blockTimestampMs", "logs"),
        ("rpc_eth_subscribe_transactionReceipts_logs_blockTimestampMs", "transactionReceipts"),
    ];
    let Some(el_ws_rpc) = el_ws_rpc else {
        for (name, _) in SUBSCRIPTIONS {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "matching WebSocket event",
                "standard execution WebSocket RPC is not configured",
            );
        }
        return;
    };
    if matches!(target, DenimCheckTarget::BlockHash(_)) {
        for (name, _) in SUBSCRIPTIONS {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "matching WebSocket event",
                "historical block subscriptions cannot be replayed",
            );
        }
        return;
    }

    let connection =
        tokio::time::timeout(Duration::from_secs(2), connect_async(el_ws_rpc.as_str())).await;
    let Ok(Ok((mut socket, _))) = connection else {
        for (name, _) in SUBSCRIPTIONS {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "accepted WebSocket subscription",
                "WebSocket connection unavailable",
            );
        }
        return;
    };

    let mut pending = HashMap::new();
    for (index, (name, kind)) in SUBSCRIPTIONS.into_iter().enumerate() {
        let id = index + 1;
        pending.insert(id as u64, (name, kind));
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "eth_subscribe",
            "params": [kind],
        });
        if socket.send(Message::Text(request.to_string().into())).await.is_err() {
            break;
        }
    }
    let mut subscriptions: HashMap<String, (&str, &str)> = HashMap::new();
    let mut completed = HashSet::new();
    let mut indeterminate = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while completed.len() < SUBSCRIPTIONS.len() {
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(message))) => message,
            _ => break,
        };
        if !message.is_text() {
            if message.is_close() {
                break;
            }
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(message.to_text().unwrap_or_default()) else {
            continue;
        };
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            let Some((name, kind)) = pending.remove(&id) else { continue };
            if let Some(error) = value.get("error") {
                report.add_check(
                    name,
                    DenimCheckStatus::Fail,
                    "accepted WebSocket subscription",
                    error.get("message").and_then(Value::as_str).unwrap_or("subscription rejected"),
                );
                completed.insert(name);
            } else if let Some(subscription) = value.get("result").and_then(Value::as_str) {
                subscriptions.insert(subscription.to_string(), (name, kind));
            } else {
                report.add_check(
                    name,
                    DenimCheckStatus::Fail,
                    "subscription identifier",
                    "malformed subscription response",
                );
                completed.insert(name);
            }
            continue;
        }
        let Some(subscription) = value.pointer("/params/subscription").and_then(Value::as_str)
        else {
            continue;
        };
        let Some(&(name, kind)) = subscriptions.get(subscription) else { continue };
        if completed.contains(name) {
            continue;
        }
        let (status, observed) = subscription_event_result(provider, kind, &value).await;
        if status == DenimCheckStatus::Indeterminate {
            indeterminate.insert(name, observed);
            continue;
        }
        report.add_check(name, status, "matching timestamped WebSocket event", observed);
        completed.insert(name);
    }
    for (name, _) in SUBSCRIPTIONS {
        if !completed.contains(name) {
            report.add_check(
                name,
                DenimCheckStatus::Indeterminate,
                "matching timestamped WebSocket event",
                if pending.values().any(|(pending_name, _)| *pending_name == name) {
                    "subscription response unavailable"
                } else if let Some(observed) = indeterminate.get(name) {
                    observed
                } else {
                    "subscription accepted; no correlated event observed"
                },
            );
        }
    }
    let _ = socket.close(None).await;
}

impl DenimReport {
    /// Appends a detailed check.
    pub fn add_check(
        &mut self,
        name: &str,
        status: DenimCheckStatus,
        expected: impl ToString,
        observed: impl ToString,
    ) {
        self.checks.push(DenimCheck {
            name: name.into(),
            status,
            expected: expected.to_string(),
            observed: observed.to_string(),
        });
    }

    /// Evaluates complete RPC observations into detailed snapshot checks.
    pub fn evaluate(input: DenimObservations) -> Self {
        let schedule = match input.activation {
            None => DenimSchedule::NotScheduled,
            Some(at) if input.timestamp < at => DenimSchedule::Scheduled,
            Some(_) => DenimSchedule::Active,
        };
        let active = schedule == DenimSchedule::Active;
        let initial = input.implementation
            == U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice());
        let empty_code_hash = keccak256([]);
        let canonical_admin =
            input.admin == U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice());
        let implementation = if input.proxy_code_hash == empty_code_hash {
            "Missing"
        } else if input.proxy_code_hash != BaseTime::PROXY_CODE_HASH || !canonical_admin {
            "Inconsistent"
        } else if input.implementation == U256::ZERO {
            "Dormant"
        } else if !input.implementation_is_address {
            "Inconsistent"
        } else if initial
            && input.implementation_code_hash == Some(BaseTime::IMPLEMENTATION_CODE_HASH)
        {
            "LinkedInitial"
        } else if initial
            || input.implementation_code_hash.is_none()
            || input.implementation_code_hash == Some(empty_code_hash)
        {
            "Inconsistent"
        } else {
            "LinkedOther"
        };
        let expected_ms = input
            .metadata_millis_part
            .map(|part| input.timestamp.wrapping_mul(1_000).wrapping_add(u64::from(part)));
        let mut report = Self {
            schedule,
            block_number: input.block_number,
            block_hash: input.block_hash,
            timestamp: input.timestamp,
            timestamp_ms: input.timestamp_ms,
            activation: input.activation,
            checks: Vec::new(),
            cursor: None,
        };
        let status = |matches| {
            if matches {
                DenimCheckStatus::Pass
            } else if active {
                DenimCheckStatus::Fail
            } else {
                DenimCheckStatus::Indeterminate
            }
        };
        for (name, matches, expected, observed) in [
            (
                "proxy_code_hash",
                input.proxy_code_hash == BaseTime::PROXY_CODE_HASH,
                BaseTime::PROXY_CODE_HASH.to_string(),
                input.proxy_code_hash.to_string(),
            ),
            (
                "proxy_admin",
                canonical_admin,
                Predeploys::PROXY_ADMIN.to_string(),
                input.admin.to_string(),
            ),
            (
                "implementation",
                matches!(implementation, "LinkedInitial" | "LinkedOther"),
                "a linked implementation with deployed code".into(),
                implementation.into(),
            ),
        ] {
            report.add_check(name, status(matches), expected, observed);
        }
        if active {
            for (name, matches, expected, observed) in [
                (
                    "metadata",
                    input.metadata_millis_part.is_some(),
                    "canonical tx[1] BaseTime deposit".into(),
                    input.metadata_error.clone().unwrap_or_else(|| {
                        input
                            .metadata_millis_part
                            .map_or_else(|| "missing".into(), |v| v.to_string())
                    }),
                ),
                (
                    "metadata_receipt",
                    input.metadata_receipt_valid == Some(true),
                    "successful receipt at index 1 in the snapshot block".into(),
                    input
                        .metadata_receipt_valid
                        .map_or_else(|| "missing".into(), |v| v.to_string()),
                ),
                (
                    "header_timestamp_ms",
                    input.timestamp_ms == expected_ms && expected_ms.is_some(),
                    expected_ms.map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.timestamp_ms.map_or_else(|| "missing".into(), |v| v.to_string()),
                ),
                (
                    "storage_millis_part",
                    Some(input.storage_millis_part) == input.metadata_millis_part,
                    input
                        .metadata_millis_part
                        .map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.storage_millis_part.to_string(),
                ),
                (
                    "getter_millis_part",
                    input.getter_millis_part == input.metadata_millis_part
                        && input.metadata_millis_part.is_some(),
                    input
                        .metadata_millis_part
                        .map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.getter_millis_part.map_or_else(
                        || {
                            input
                                .getter_millis_part_error
                                .clone()
                                .unwrap_or_else(|| "unavailable".into())
                        },
                        |v| v.to_string(),
                    ),
                ),
                (
                    "getter_timestamp_ms",
                    input.getter_timestamp_ms == expected_ms && expected_ms.is_some(),
                    expected_ms.map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.getter_timestamp_ms.map_or_else(
                        || {
                            input
                                .getter_timestamp_ms_error
                                .clone()
                                .unwrap_or_else(|| "unavailable".into())
                        },
                        |v| v.to_string(),
                    ),
                ),
            ] {
                report.add_check(name, status(matches), expected, observed);
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_consensus::Sealable;
    use alloy_primitives::{B256, U256};
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use base_common_consensus::{BaseTxEnvelope, Predeploys, TxDeposit};
    use base_common_evm::BaseTime;
    use base_common_genesis::RollupConfig;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use url::Url;

    use super::*;

    async fn denim_rpc_fixture(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        requests.lock().unwrap().push(request.clone());
        let hash = B256::repeat_byte(0x42);
        let result = match request["method"].as_str().unwrap() {
            "optimism_rollupConfig" => {
                let mut config = serde_json::to_value(RollupConfig::default()).unwrap();
                config["l2_chain_id"] = json!(8453);
                config["base"]["denim"] = json!(20);
                config
            }
            "eth_getBlockByHash"
            | "eth_getBlockByNumber"
            | "eth_getHeaderByHash"
            | "eth_getHeaderByNumber" => json!({
                "hash": hash,
                "parentHash": B256::ZERO,
                "sha3Uncles": B256::ZERO,
                "miner": Address::ZERO,
                "stateRoot": B256::ZERO,
                "transactionsRoot": B256::ZERO,
                "receiptsRoot": B256::ZERO,
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "difficulty": "0x0",
                "number": "0xa",
                "gasLimit": "0x1c9c380",
                "gasUsed": "0x0",
                "timestamp": "0xa",
                "timestampMs": "0x2710",
                "extraData": "0x",
                "mixHash": B256::ZERO,
                "nonce": "0x0000000000000000",
                "baseFeePerGas": "0x1",
                "size": "0x0",
                "totalDifficulty": "0x0",
                "uncles": [],
                "transactions": []
            }),
            "eth_getCode" => json!(BaseTime::proxy_bytecode()),
            "eth_getStorageAt" if request["params"][1] == json!(BaseTime::ADMIN_SLOT) => {
                json!(format!("{:#066x}", U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice())))
            }
            "eth_getStorageAt" => json!(format!("{:#066x}", U256::ZERO)),
            method => panic!("unexpected RPC method {method}"),
        };
        Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
    }

    async fn raw_error_fixture(Json(request): Json<Value>) -> (StatusCode, Json<Value>) {
        assert_eq!(request["method"], "eth_test");
        assert_eq!(request["params"], json!(["exact", {"shape": true}]));
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32601, "message": "method not found"}
            })),
        )
    }

    fn rpc_deposit(
        transaction: BaseTxEnvelope,
        block_hash: B256,
        block_number: u64,
        transaction_index: u64,
    ) -> Value {
        let transaction_hash = transaction.tx_hash().to_string();
        let mut value = serde_json::to_value(transaction).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("hash".into(), Value::String(transaction_hash));
        object.insert("blockHash".into(), json!(block_hash));
        object.insert("blockNumber".into(), json!(format!("0x{block_number:x}")));
        object.insert("transactionIndex".into(), json!(format!("0x{transaction_index:x}")));
        object.insert("blockTimestamp".into(), json!("0xa"));
        object.insert("blockTimestampMs".into(), json!("0x27d8"));
        object.insert("depositReceiptVersion".into(), json!("0x1"));
        object.insert("gasPrice".into(), json!("0x0"));
        object.insert("nonce".into(), json!(format!("0x{transaction_index:x}")));
        value
    }

    fn rpc_block(block_hash: B256, block_number: u64, log_transaction: bool) -> Value {
        let l1_info: BaseTxEnvelope = TxDeposit::default().seal_slow().into();
        let base_time: BaseTxEnvelope =
            BaseTimeUpdateTx::new(200).unwrap().into_deposit_tx(block_number).into();
        let mut transactions = vec![
            rpc_deposit(l1_info, block_hash, block_number, 0),
            rpc_deposit(base_time, block_hash, block_number, 1),
        ];
        if log_transaction {
            let transaction: BaseTxEnvelope =
                TxDeposit { source_hash: B256::repeat_byte(0x33), ..Default::default() }
                    .seal_slow()
                    .into();
            transactions.push(rpc_deposit(transaction, block_hash, block_number, 2));
        }
        json!({
            "hash": block_hash,
            "parentHash": B256::ZERO,
            "sha3Uncles": B256::ZERO,
            "miner": Address::ZERO,
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": format!("0x{block_number:x}"),
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0xa",
            "timestampMs": "0x27d8",
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "size": "0x0",
            "totalDifficulty": "0x0",
            "uncles": [],
            "transactions": transactions
        })
    }

    async fn denim_log_rpc_fixture(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        const LATEST_NUMBER: u64 = 65_000;
        const LOG_NUMBER: u64 = 40;
        let latest_hash = B256::repeat_byte(0x42);
        let log_hash = B256::repeat_byte(0x24);
        let latest_block = rpc_block(latest_hash, LATEST_NUMBER, false);
        let log_block = rpc_block(log_hash, LOG_NUMBER, true);
        let latest_transaction = latest_block["transactions"][1].clone();
        let log_transaction = log_block["transactions"][2].clone();
        let log = json!({
            "address": Address::ZERO,
            "topics": [],
            "data": "0x",
            "blockHash": log_hash,
            "blockNumber": "0x28",
            "blockTimestampMs": "0x27d8",
            "transactionHash": log_transaction["hash"],
            "transactionIndex": "0x2",
            "logIndex": "0x0",
            "removed": false
        });
        requests.lock().unwrap().push(request.clone());
        let params = &request["params"];
        if request["method"] == "eth_getLogs" {
            let from = params[0]["fromBlock"].as_str().and_then(parse_quantity);
            let to = params[0]["toBlock"].as_str().and_then(parse_quantity);
            if matches!((from, to), (Some(from), Some(to)) if to - from + 1 > 8_192) {
                return Json(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32005, "message": "block range too large"}
                }));
            }
        }
        let result = match request["method"].as_str().unwrap() {
            "eth_getBlockByHash" if params[0] == json!(log_hash) => log_block,
            "eth_getBlockByHash"
            | "eth_getBlockByNumber"
            | "eth_getHeaderByHash"
            | "eth_getHeaderByNumber" => latest_block,
            "eth_getTransactionByHash"
            | "eth_getTransactionByBlockHashAndIndex"
            | "eth_getTransactionByBlockNumberAndIndex" => latest_transaction,
            "eth_getLogs" if params[0].get("blockHash") == Some(&json!(log_hash)) => {
                json!([log])
            }
            "eth_getLogs" if params[0].get("fromBlock").is_some() => {
                if params[0]["fromBlock"] == "0x0" { json!([log]) } else { json!([]) }
            }
            "eth_getLogs" => json!([]),
            "eth_newFilter" => json!("0xf56a19202c3fde509c9b1f806c0b12af"),
            "eth_getFilterChanges" | "eth_getFilterLogs" => json!([log]),
            "eth_uninstallFilter" => json!(true),
            "eth_getTransactionReceipt" => json!({
                "transactionHash": log_transaction["hash"],
                "blockHash": log_hash,
                "blockNumber": "0x28",
                "logs": [log]
            }),
            "eth_getBlockReceipts" => json!([{
                "blockHash": log_hash,
                "blockNumber": "0x28",
                "logs": [log]
            }]),
            method => panic!("unexpected RPC method {method}"),
        };
        Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
    }

    async fn denim_log_ws_fixture(listener: TcpListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for _ in 0..3 {
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            let id = request["id"].as_u64().unwrap();
            socket
                .send(Message::Text(
                    json!({"jsonrpc":"2.0","id":id,"result":format!("0x{id:x}")})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
        let block_hash = B256::repeat_byte(0x24);
        let log = json!({
            "blockHash": block_hash,
            "blockTimestampMs": "0x27d8"
        });
        for notification in [
            json!({
                "jsonrpc": "2.0",
                "method": "eth_subscription",
                "params": {"subscription":"0x3","result":[{
                    "blockHash": block_hash,
                    "blockNumber": "0x28",
                    "logs": []
                }]}
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "eth_subscription",
                "params": {"subscription":"0x1","result":{
                    "hash": block_hash,
                    "timestampMs": "0x27d8"
                }}
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "eth_subscription",
                "params": {"subscription":"0x2","result":log}
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "eth_subscription",
                "params": {"subscription":"0x3","result":[{
                    "blockHash": block_hash,
                    "blockNumber": "0x28",
                    "logs": [log]
                }]}
            }),
        ] {
            socket.send(Message::Text(notification.to_string().into())).await.unwrap();
        }
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    }

    fn observations(activation: Option<u64>, timestamp: u64) -> DenimObservations {
        let part = 200;
        DenimObservations {
            activation,
            block_number: 10,
            block_hash: B256::ZERO,
            timestamp,
            timestamp_ms: Some(timestamp.wrapping_mul(1_000).wrapping_add(u64::from(part))),
            proxy_code_hash: BaseTime::PROXY_CODE_HASH,
            admin: U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
            implementation: U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice()),
            implementation_is_address: true,
            implementation_code_hash: Some(BaseTime::IMPLEMENTATION_CODE_HASH),
            storage_millis_part: part,
            metadata_millis_part: Some(part),
            metadata_error: None,
            metadata_receipt_valid: Some(true),
            getter_millis_part: Some(part),
            getter_millis_part_error: None,
            getter_timestamp_ms: Some(timestamp.wrapping_mul(1_000).wrapping_add(u64::from(part))),
            getter_timestamp_ms_error: None,
        }
    }

    #[tokio::test]
    async fn block_hash_target_pins_snapshot_and_state_reads() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router =
            Router::new().route("/", post(denim_rpc_fixture)).with_state(Arc::clone(&requests));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let url = Url::parse(&format!("http://{address}")).unwrap();
        let hash = B256::repeat_byte(0x42);
        let report = DenimChecker
            .check(&url, &url, None, DenimCheckTarget::BlockHash(hash), None)
            .await
            .unwrap();

        assert_eq!(report.schedule, DenimSchedule::Scheduled);
        assert_eq!(report.block_number, 10);
        assert_eq!(report.block_hash, hash);

        let requests = requests.lock().unwrap();
        let hash_reads: Vec<_> = requests
            .iter()
            .filter(|request| {
                matches!(
                    request["method"].as_str(),
                    Some("eth_getBlockByHash" | "eth_getHeaderByHash")
                )
            })
            .collect();
        assert_eq!(hash_reads.len(), 2);
        assert_eq!(
            hash_reads.iter().filter(|request| request["params"][0] == json!(hash)).count(),
            2
        );
        for request in requests.iter().filter(|request| {
            matches!(request["method"].as_str(), Some("eth_getCode" | "eth_getStorageAt"))
        }) {
            assert_eq!(request["params"].as_array().unwrap().last(), Some(&json!(hash)));
        }
        assert_eq!(requests.iter().filter(|request| request["method"] == "eth_getCode").count(), 1);
        assert_eq!(
            requests.iter().filter(|request| request["method"] == "eth_getStorageAt").count(),
            3
        );
        server.abort();
    }

    #[test]
    fn schedule_selection_and_inactive_metadata_are_stable() {
        let unscheduled = DenimReport::evaluate(observations(None, 10));
        assert_eq!(unscheduled.schedule, DenimSchedule::NotScheduled);
        assert!(unscheduled.checks.iter().all(|check| check.status != DenimCheckStatus::Fail));

        let scheduled = DenimReport::evaluate(observations(Some(20), 10));
        assert_eq!(scheduled.schedule, DenimSchedule::Scheduled);
    }

    #[test]
    fn active_snapshot_requires_canonical_metadata_and_state() {
        let mut input = observations(Some(10), 10);
        input.storage_millis_part = 400;
        let report = DenimReport::evaluate(input);
        assert_eq!(report.schedule, DenimSchedule::Active);
        assert_eq!(
            report.checks.iter().find(|check| check.name == "storage_millis_part").unwrap().status,
            DenimCheckStatus::Fail
        );
    }

    #[test]
    fn governance_changed_implementation_with_code_is_permitted() {
        let mut input = observations(Some(10), 10);
        input.implementation = U256::from(1);
        input.implementation_code_hash = Some(B256::with_last_byte(1));
        let report = DenimReport::evaluate(input);
        assert_eq!(
            report.checks.iter().find(|check| check.name == "implementation").unwrap().status,
            DenimCheckStatus::Pass
        );
    }

    #[test]
    fn malformed_or_empty_implementation_fails() {
        let mut malformed = observations(Some(10), 10);
        malformed.implementation_is_address = false;
        let mut empty = observations(Some(10), 10);
        empty.implementation = U256::from(1);
        empty.implementation_code_hash = Some(keccak256([]));
        let mut unavailable = observations(Some(10), 10);
        unavailable.implementation = U256::from(1);
        unavailable.implementation_code_hash = None;

        for input in [malformed, empty, unavailable] {
            let check = DenimReport::evaluate(input)
                .checks
                .into_iter()
                .find(|check| check.name == "implementation")
                .unwrap();
            assert_eq!(check.status, DenimCheckStatus::Fail);
            assert_eq!(check.observed, "Inconsistent");
        }
    }

    #[test]
    fn unavailable_getters_fail_instead_of_comparing_equal() {
        let mut input = observations(Some(10), 10);
        input.metadata_millis_part = None;
        input.metadata_error = Some("missing metadata".into());
        input.getter_millis_part = None;
        input.getter_millis_part_error = Some("execution reverted".into());
        input.getter_timestamp_ms = None;
        input.getter_timestamp_ms_error = Some("execution reverted".into());
        let report = DenimReport::evaluate(input);

        let getter_checks: Vec<_> =
            report.checks.iter().filter(|check| check.name.starts_with("getter_")).collect();
        assert_eq!(getter_checks.len(), 2);
        assert!(getter_checks.iter().all(|check| check.status == DenimCheckStatus::Fail));
    }

    #[test]
    fn timestamp_comparison_uses_uint64_wrapping_semantics() {
        let timestamp = u64::MAX / 1_000 + 1;
        let mut input = observations(Some(0), timestamp);
        let expected = timestamp.wrapping_mul(1_000).wrapping_add(200);
        input.timestamp_ms = Some(expected);
        input.getter_timestamp_ms = Some(expected);

        assert!(
            DenimReport::evaluate(input)
                .checks
                .iter()
                .filter(|check| check.name.starts_with("getter_")
                    || check.name == "header_timestamp_ms")
                .all(|check| check.status == DenimCheckStatus::Pass)
        );
    }

    #[test]
    fn report_serializes_consumer_dimensions_and_failure_context() {
        let mut input = observations(Some(10), 10);
        input.metadata_receipt_valid = Some(false);
        let value = serde_json::to_value(DenimReport::evaluate(input)).unwrap();
        assert_eq!(value["schedule"], "active");
        assert_eq!(
            value["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["name"] == "metadata_receipt")
                .unwrap()["status"],
            "fail"
        );
        for removed in ["endpoint", "blockNumber", "blockHash", "remediation"] {
            assert!(value["checks"][0].get(removed).is_none());
        }
        for removed in ["overall", "identity", "installation", "metadata", "rpcHttp", "chainId"] {
            assert!(value.get(removed).is_none());
        }
        assert!(value.get("cursor").is_none());
    }

    #[test]
    fn cadence_classifies_boundary_exact_gap_and_wrong_gap() {
        assert_eq!(cadence_status(None, Some(1_000), true), DenimCheckStatus::Indeterminate);
        assert_eq!(cadence_status(Some(1_000), Some(1_200), true), DenimCheckStatus::Pass);
        assert_eq!(cadence_status(Some(1_000), Some(1_400), true), DenimCheckStatus::Fail);
        assert_eq!(
            cadence_status(Some(1_000), Some(1_200), false),
            DenimCheckStatus::Indeterminate
        );
    }

    #[test]
    fn raw_object_classifies_field_and_method_evidence() {
        let mut report = DenimReport::evaluate(observations(Some(10), 10));
        let identity = [("hash", B256::repeat_byte(0x42).to_string())];
        check_wire_object(
            &mut report,
            "pass",
            RawOutcome::Value(json!({"hash":B256::repeat_byte(0x42),"timestampMs":"0x2710"})),
            "timestampMs",
            10_000,
            &identity,
        );
        check_wire_object(
            &mut report,
            "missing",
            RawOutcome::Value(json!({"hash":B256::repeat_byte(0x42)})),
            "timestampMs",
            10_000,
            &identity,
        );
        check_wire_object(
            &mut report,
            "method",
            RawOutcome::MethodError("method not found".into()),
            "timestampMs",
            10_000,
            &identity,
        );
        check_wire_logs(&mut report, "empty", RawOutcome::Value(json!([])), 10_000, "0x42");

        let statuses: Vec<_> = report.checks.iter().rev().take(4).map(|c| c.status).collect();
        assert_eq!(
            statuses,
            vec![
                DenimCheckStatus::Fail,
                DenimCheckStatus::Fail,
                DenimCheckStatus::Fail,
                DenimCheckStatus::Pass,
            ]
        );
    }

    #[tokio::test]
    async fn raw_call_preserves_params_and_non_success_rpc_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/", post(raw_error_fixture))).await.unwrap()
        });
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(Url::parse(&format!("http://{address}")).unwrap());

        let outcome = raw_call(&provider, "eth_test", json!(["exact", {"shape": true}])).await;

        assert!(matches!(outcome, RawOutcome::MethodError(error) if error == "method not found"));
        server.abort();
    }

    #[tokio::test]
    async fn log_checks_use_a_historical_log_transaction() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router =
            Router::new().route("/", post(denim_log_rpc_fixture)).with_state(Arc::clone(&requests));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(Url::parse(&format!("http://{address}")).unwrap());
        let latest_hash = B256::repeat_byte(0x42);
        let latest_block = rpc_block(latest_hash, 65_000, false);
        let block: <Base as Network>::BlockResponse = serde_json::from_value(latest_block).unwrap();
        let mut report = DenimReport::evaluate(DenimObservations {
            block_number: 65_000,
            block_hash: latest_hash,
            ..observations(Some(0), 10)
        });

        append_wire_checks(&provider, &mut report, &block, None, DenimCheckTarget::Latest).await;

        for name in [
            "rpc_eth_getLogs_blockTimestampMs",
            "rpc_eth_getFilterChanges_blockTimestampMs",
            "rpc_eth_getFilterLogs_blockTimestampMs",
            "rpc_eth_getTransactionReceipt_logs_blockTimestampMs",
            "rpc_eth_getBlockReceipts_logs_blockTimestampMs",
        ] {
            let check = report.checks.iter().find(|check| check.name == name).unwrap();
            assert_eq!(check.status, DenimCheckStatus::Pass, "{check:?}");
        }
        let log_hash = B256::repeat_byte(0x24);
        let log_transaction = rpc_block(log_hash, 40, true)["transactions"][2]["hash"].clone();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|request| {
            request["method"] == "eth_getTransactionReceipt"
                && request["params"] == json!([log_transaction])
        }));
        assert!(requests.iter().any(|request| {
            request["method"] == "eth_getLogs"
                && request["params"][0]["blockHash"] == json!(log_hash)
        }));
        assert!(
            requests
                .iter()
                .filter(|request| {
                    request["method"] == "eth_getLogs"
                        && request["params"][0].get("fromBlock").is_some()
                })
                .count()
                > 1
        );
        server.abort();
    }

    #[tokio::test]
    async fn receipt_subscription_waits_for_a_log_bearing_event() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http_listener.local_addr().unwrap();
        let router =
            Router::new().route("/", post(denim_log_rpc_fixture)).with_state(Arc::clone(&requests));
        let http_server =
            tokio::spawn(async move { axum::serve(http_listener, router).await.unwrap() });
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();
        let ws_server = tokio::spawn(denim_log_ws_fixture(ws_listener));
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<Base>()
            .connect_http(Url::parse(&format!("http://{http_address}")).unwrap());
        let ws_url = Url::parse(&format!("ws://{ws_address}")).unwrap();
        let mut report = DenimReport::evaluate(observations(Some(0), 10));

        append_subscription_checks(&provider, &mut report, Some(&ws_url), DenimCheckTarget::Latest)
            .await;

        for name in [
            "rpc_eth_subscribe_newHeads_timestampMs",
            "rpc_eth_subscribe_logs_blockTimestampMs",
            "rpc_eth_subscribe_transactionReceipts_logs_blockTimestampMs",
        ] {
            let check = report.checks.iter().find(|check| check.name == name).unwrap();
            assert_eq!(check.status, DenimCheckStatus::Pass, "{check:?}");
        }
        http_server.abort();
        ws_server.await.unwrap();
    }
}
