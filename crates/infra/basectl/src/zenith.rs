//! Reusable Zenith activation checks over one hash-pinned L2 snapshot.

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
use alloy_rpc_types_eth::{BlockNumberOrTag, SyncStatus as EthSyncStatus, TransactionRequest};
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

/// Snapshot selection for a Zenith check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZenithCheckTarget {
    /// Resolve the latest block once, then pin all reads to its hash.
    Latest,
    /// Check the snapshot identified by this block hash.
    BlockHash(B256),
}

/// Last hash-pinned snapshot checked by a caller that polls `Latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZenithCheckCursor {
    /// Checked block number.
    pub block_number: u64,
    /// Checked block hash.
    pub block_hash: B256,
}

/// Overall Zenith health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenithStatus {
    /// Every required invariant holds.
    Healthy,
    /// One or more required invariants failed.
    Broken,
    /// The available observations cannot establish health.
    Indeterminate,
}

/// Zenith schedule state at the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenithSchedule {
    /// The CL has no Zenith activation configured.
    NotScheduled,
    /// Zenith is configured after the snapshot timestamp.
    Scheduled,
    /// Zenith is active at the snapshot timestamp.
    Active,
    /// The observed schedule is internally inconsistent.
    Inconsistent,
}

/// `BaseTime` installation state at the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenithInstallation {
    /// No code exists at the reserved proxy address.
    Missing,
    /// The canonical proxy exists but has no implementation linkage.
    Dormant,
    /// The canonical initial implementation is linked.
    LinkedInitial,
    /// A nonzero governance-selected implementation is linked.
    LinkedOther,
    /// Proxy, admin, implementation, or bytecode observations conflict.
    Inconsistent,
}

/// Status of a report dimension or individual check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenithCheckStatus {
    /// The invariant holds.
    Pass,
    /// The invariant does not hold.
    Fail,
    /// The invariant does not apply or cannot be established yet.
    Indeterminate,
}

/// HTTP RPC health for the completed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenithRpcStatus {
    /// Every required HTTP RPC read succeeded.
    Pass,
    /// Required reads succeeded with reduced coverage.
    Degraded,
    /// A required HTTP RPC read failed.
    Fail,
}

/// One evaluated Zenith invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenithCheck {
    /// Stable check name.
    pub name: String,
    /// Check result.
    pub status: ZenithCheckStatus,
    /// RPC endpoint responsible for the observation.
    pub endpoint: String,
    /// Snapshot block number.
    pub block_number: u64,
    /// Snapshot block hash.
    pub block_hash: B256,
    /// Expected value.
    pub expected: String,
    /// Observed value.
    pub observed: String,
    /// Operator action when the check fails.
    pub remediation: String,
}

/// Serializable report for one hash-pinned Zenith snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenithReport {
    /// Overall health.
    pub overall: ZenithStatus,
    /// EL, CL, and configured network identity status.
    pub identity: ZenithCheckStatus,
    /// Activation state.
    pub schedule: ZenithSchedule,
    /// Predeploy installation state.
    pub installation: ZenithInstallation,
    /// Active-block metadata and state agreement.
    pub metadata: ZenithCheckStatus,
    /// HTTP RPC health.
    pub rpc_http: ZenithRpcStatus,
    /// EL chain ID.
    pub chain_id: u64,
    /// Explicit snapshot block number.
    pub block_number: u64,
    /// Explicit snapshot block hash.
    pub block_hash: B256,
    /// Snapshot timestamp in seconds.
    pub timestamp: u64,
    /// Snapshot timestamp in milliseconds, when exposed.
    pub timestamp_ms: Option<u64>,
    /// CL Zenith activation timestamp.
    pub activation: Option<u64>,
    /// Detailed checks.
    pub checks: Vec<ZenithCheck>,
    /// Safe polling cursor, absent when cadence ancestry was incomplete.
    #[serde(skip)]
    pub cursor: Option<ZenithCheckCursor>,
}

/// Pure observations used to evaluate a Zenith report.
#[derive(Debug, Clone)]
pub struct ZenithObservations {
    /// EL RPC endpoint.
    pub el_endpoint: String,
    /// CL RPC endpoint.
    pub cl_endpoint: String,
    /// EL chain ID.
    pub el_chain_id: u64,
    /// CL chain ID.
    pub cl_chain_id: u64,
    /// Chain ID expected by basectl configuration.
    pub expected_chain_id: Option<u64>,
    /// CL Zenith activation timestamp.
    pub activation: Option<u64>,
    /// EL sync state.
    pub el_syncing: bool,
    /// Snapshot block number.
    pub block_number: u64,
    /// Snapshot block hash.
    pub block_hash: B256,
    /// Snapshot timestamp in seconds.
    pub timestamp: u64,
    /// Header timestamp in milliseconds.
    pub timestamp_ms: Option<u64>,
    /// Whether all number/hash block and header views agreed.
    pub snapshot_consistent: bool,
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

/// Zenith checker configuration and RPC entry point.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZenithChecker {
    expected_chain_id: Option<u64>,
}

impl ZenithChecker {
    /// Creates a checker with the chain ID expected by basectl configuration.
    pub const fn new(expected_chain_id: Option<u64>) -> Self {
        Self { expected_chain_id }
    }

    /// Checks one snapshot using the supplied execution and consensus RPCs.
    pub async fn check(
        &self,
        el_rpc: &Url,
        cl_rpc: &Url,
        target: ZenithCheckTarget,
    ) -> Result<ZenithReport> {
        let mut report = self.check_since(el_rpc, cl_rpc, None, target, None).await?;
        report
            .checks
            .retain(|check| !check.name.starts_with("rpc_") && check.name != "cadence_200ms");
        report.overall =
            if report.checks.iter().any(|check| check.status == ZenithCheckStatus::Fail) {
                ZenithStatus::Broken
            } else {
                ZenithStatus::Healthy
            };
        Ok(report)
    }

    /// Checks a snapshot, including every cadence edge after `previous` for `Latest`.
    pub async fn check_since(
        &self,
        el_rpc: &Url,
        cl_rpc: &Url,
        el_ws_rpc: Option<&Url>,
        target: ZenithCheckTarget,
        previous: Option<ZenithCheckCursor>,
    ) -> Result<ZenithReport> {
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
        let (el_chain_id, syncing) = tokio::try_join!(provider.get_chain_id(), provider.syncing())?;

        let hash = match target {
            ZenithCheckTarget::Latest => {
                provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .full()
                    .await?
                    .ok_or_else(|| anyhow!("latest block not found"))?
                    .header
                    .hash
            }
            ZenithCheckTarget::BlockHash(hash) => hash,
        };
        let block_id = BlockId::Hash(hash.into());
        let full_hash = provider
            .get_block(block_id)
            .full()
            .await?
            .ok_or_else(|| anyhow!("snapshot block by hash not found"))?;
        let number = full_hash.header.number;
        let id_number = BlockId::Number(BlockNumberOrTag::Number(number));
        let (full_number, header_number, header_hash) = tokio::try_join!(
            provider.get_block(id_number).full(),
            provider.get_header(id_number),
            provider.get_header(block_id),
        )?;
        let full_number =
            full_number.ok_or_else(|| anyhow!("snapshot block by number not found"))?;
        let header_number =
            header_number.ok_or_else(|| anyhow!("snapshot header by number not found"))?;
        let header_hash =
            header_hash.ok_or_else(|| anyhow!("snapshot header by hash not found"))?;
        let same_block = |block: &<Base as Network>::BlockResponse| {
            block.header.hash == hash
                && block.header.number == number
                && block.header.timestamp == full_hash.header.timestamp
                && block.header.timestamp_ms == full_hash.header.timestamp_ms
        };
        let same_header = |header: &<Base as Network>::HeaderResponse| {
            header.hash == hash
                && header.number == number
                && header.timestamp == full_hash.header.timestamp
                && header.timestamp_ms == full_hash.header.timestamp_ms
        };
        let transactions_match = full_number
            .transactions
            .txns()
            .map(TransactionResponse::tx_hash)
            .eq(full_hash.transactions.txns().map(TransactionResponse::tx_hash));
        let snapshot_consistent = same_block(&full_number)
            && same_block(&full_hash)
            && same_header(&header_number)
            && same_header(&header_hash)
            && transactions_match;

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

        let active = rollup.upgrades.base.zenith.is_some_and(|at| full_hash.header.timestamp >= at);
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

        let mut report = ZenithReport::evaluate(ZenithObservations {
            el_endpoint: el_rpc.to_string(),
            cl_endpoint: cl_rpc.to_string(),
            el_chain_id,
            cl_chain_id: rollup.l2_chain_id.id(),
            expected_chain_id: self.expected_chain_id,
            activation: rollup.upgrades.base.zenith,
            el_syncing: !matches!(syncing, EthSyncStatus::None),
            block_number: number,
            block_hash: hash,
            timestamp: full_hash.header.timestamp,
            timestamp_ms: full_hash.header.timestamp_ms,
            snapshot_consistent,
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
        let raw = RawRpc::new(el_rpc)?;
        let cadence_complete = append_cadence_check(&provider, &mut report, target, previous).await;
        let cadence_passed = report
            .checks
            .iter()
            .find(|check| check.name == "cadence_200ms")
            .is_some_and(|check| check.status == ZenithCheckStatus::Pass);
        report.cursor = (cadence_complete && cadence_passed).then_some(ZenithCheckCursor {
            block_number: report.block_number,
            block_hash: report.block_hash,
        });
        append_wire_checks(&provider, &raw, &mut report, &full_hash, el_ws_rpc, target).await;
        let http_checks = report.checks.iter().filter(|check| {
            check.name.starts_with("rpc_eth_") && !check.name.contains("subscribe")
        });
        report.rpc_http = if report.rpc_http == ZenithRpcStatus::Fail
            || http_checks.clone().any(|check| check.status == ZenithCheckStatus::Fail)
        {
            ZenithRpcStatus::Fail
        } else if http_checks.clone().any(|check| check.status == ZenithCheckStatus::Indeterminate)
        {
            ZenithRpcStatus::Degraded
        } else {
            ZenithRpcStatus::Pass
        };
        report.overall = if report
            .checks
            .iter()
            .any(|check| check.status == ZenithCheckStatus::Fail)
        {
            ZenithStatus::Broken
        } else if report.checks.iter().any(|check| check.status == ZenithCheckStatus::Indeterminate)
        {
            ZenithStatus::Indeterminate
        } else {
            ZenithStatus::Healthy
        };
        Ok(report)
    }
}

#[derive(Debug)]
enum RawOutcome {
    Value(Value),
    MethodError(String),
    Unavailable(String),
}

struct RawRpc {
    endpoint: Url,
    client: alloy_transport_http::reqwest::Client,
}

impl RawRpc {
    fn new(endpoint: &Url) -> Result<Self> {
        Ok(Self {
            endpoint: endpoint.clone(),
            client: alloy_transport_http::reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    async fn call(&self, method: &str, params: Value) -> RawOutcome {
        let response = match self
            .client
            .post(self.endpoint.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                return RawOutcome::Unavailable("transport, authentication, or timeout".into());
            }
        };
        let body: Value = match response.json().await {
            Ok(body) => body,
            Err(_) => return RawOutcome::Unavailable("invalid RPC response".into()),
        };
        if let Some(error) = body.get("error") {
            return RawOutcome::MethodError(
                error.get("message").and_then(Value::as_str).unwrap_or("RPC error").to_string(),
            );
        }
        body.get("result").map_or_else(
            || RawOutcome::Unavailable("RPC response has no result".into()),
            |value| RawOutcome::Value(value.clone()),
        )
    }
}

fn add_observation(
    report: &mut ZenithReport,
    name: &str,
    status: ZenithCheckStatus,
    expected: &str,
    observed: impl ToString,
) {
    report.checks.push(ZenithCheck {
        name: name.into(),
        status,
        endpoint: String::new(),
        block_number: report.block_number,
        block_hash: report.block_hash,
        expected: expected.into(),
        observed: observed.to_string(),
        remediation: "upgrade or configure the execution RPC".into(),
    });
}

fn check_wire_object(
    report: &mut ZenithReport,
    name: &str,
    outcome: RawOutcome,
    field: &str,
    expected_timestamp: u64,
    identity: &[(&str, String)],
) {
    match outcome {
        RawOutcome::Value(value) if value.is_null() => add_observation(
            report,
            name,
            ZenithCheckStatus::Fail,
            field,
            "known snapshot object missing",
        ),
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
            add_observation(
                report,
                name,
                if pass { ZenithCheckStatus::Pass } else { ZenithCheckStatus::Fail },
                &format!("{field}=0x{expected_timestamp:x} with pinned identity"),
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
            add_observation(report, name, ZenithCheckStatus::Fail, field, error)
        }
        RawOutcome::Unavailable(error) => {
            add_observation(report, name, ZenithCheckStatus::Indeterminate, field, error)
        }
    }
}

fn check_wire_logs(
    report: &mut ZenithReport,
    name: &str,
    outcome: RawOutcome,
    expected_timestamp: u64,
    block_hash: &str,
) {
    match outcome {
        RawOutcome::Value(value) => {
            let Some(logs) = value.as_array() else {
                add_observation(
                    report,
                    name,
                    ZenithCheckStatus::Fail,
                    "log array",
                    "invalid result",
                );
                return;
            };
            if logs.is_empty() {
                add_observation(
                    report,
                    name,
                    ZenithCheckStatus::Indeterminate,
                    "at least one pinned log",
                    "no logs in pinned block",
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
            add_observation(
                report,
                name,
                if valid { ZenithCheckStatus::Pass } else { ZenithCheckStatus::Fail },
                "blockTimestampMs with pinned blockHash",
                if valid { "conformant" } else { "missing, wrong, or mismatched log field" },
            );
        }
        RawOutcome::MethodError(error) => {
            add_observation(report, name, ZenithCheckStatus::Fail, "blockTimestampMs", error)
        }
        RawOutcome::Unavailable(error) => add_observation(
            report,
            name,
            ZenithCheckStatus::Indeterminate,
            "blockTimestampMs",
            error,
        ),
    }
}

fn parse_quantity(value: &str) -> Option<u64> {
    let digits = value.strip_prefix("0x")?;
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn cadence_status(parent: Option<u64>, child: Option<u64>, contiguous: bool) -> ZenithCheckStatus {
    match (parent, child, contiguous) {
        (Some(parent), Some(child), true) if parent.checked_add(200) == Some(child) => {
            ZenithCheckStatus::Pass
        }
        (Some(_), Some(_), true) => ZenithCheckStatus::Fail,
        _ => ZenithCheckStatus::Indeterminate,
    }
}

async fn append_cadence_check<P: Provider<Base>>(
    provider: &P,
    report: &mut ZenithReport,
    target: ZenithCheckTarget,
    previous: Option<ZenithCheckCursor>,
) -> bool {
    if previous.is_some_and(|cursor| {
        matches!(target, ZenithCheckTarget::Latest)
            && cursor.block_number == report.block_number
            && cursor.block_hash == report.block_hash
    }) {
        add_observation(
            report,
            "cadence_200ms",
            ZenithCheckStatus::Pass,
            "every canonical parent-child edge is exactly +200ms",
            "no new blocks",
        );
        return true;
    }
    let stop = match (target, previous) {
        (ZenithCheckTarget::Latest, Some(cursor)) => Some(cursor),
        _ => None,
    };
    let mut hash = report.block_hash;
    let mut child_ms = None;
    let mut status = ZenithCheckStatus::Pass;
    let mut complete = false;
    let mut checked_edge = false;
    let mut child_number = None;
    let mut replacement_cursor = false;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(hash) {
            status = ZenithCheckStatus::Indeterminate;
            break;
        }
        let block = match provider.get_block(BlockId::Hash(hash.into())).full().await {
            Ok(Some(block)) => block,
            _ => {
                status = ZenithCheckStatus::Indeterminate;
                break;
            }
        };
        if block.header.hash != hash {
            status = ZenithCheckStatus::Indeterminate;
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
            status = ZenithCheckStatus::Fail;
        }
        let mut edge_checked = false;
        if active && child_ms.is_some() {
            if child_number != block.header.number.checked_add(1) {
                status = ZenithCheckStatus::Indeterminate;
                break;
            }
            edge_checked = true;
            checked_edge = true;
            let edge = cadence_status(millis, child_ms, true);
            if edge != ZenithCheckStatus::Pass && status != ZenithCheckStatus::Fail {
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
            status = ZenithCheckStatus::Indeterminate;
            break;
        }
        if stop.is_none() && checked_edge {
            complete = true;
            break;
        }
        if !active {
            if stop.is_some() {
                status = ZenithCheckStatus::Indeterminate;
            } else {
                complete = true;
            }
            break;
        }
        child_ms = millis;
        child_number = Some(block.header.number);
        hash = block.header.parent_hash;
    }
    if status == ZenithCheckStatus::Pass && !checked_edge {
        status = ZenithCheckStatus::Indeterminate;
    }
    add_observation(
        report,
        "cadence_200ms",
        status,
        "every canonical parent-child edge is exactly +200ms",
        match status {
            ZenithCheckStatus::Pass => "exact +200ms progression",
            ZenithCheckStatus::Fail => "wrong timestamp gap",
            ZenithCheckStatus::Indeterminate => "activation boundary, missing range, or reorg",
        },
    );
    complete
}

async fn append_wire_checks<P: Provider<Base>>(
    provider: &P,
    raw: &RawRpc,
    report: &mut ZenithReport,
    block: &<Base as Network>::BlockResponse,
    el_ws_rpc: Option<&Url>,
    target: ZenithCheckTarget,
) {
    let transactions =
        block.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect::<Vec<_>>();
    let Some(timestamp_ms) =
        BaseTimeUpdateTx::extract_from_transactions(&transactions, report.block_number)
            .ok()
            .map(|metadata| report.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part()))
    else {
        for name in [
            "rpc_eth_getBlockByHash_timestampMs",
            "rpc_eth_getBlockByNumber_timestampMs",
            "rpc_eth_getHeaderByHash_timestampMs",
            "rpc_eth_getHeaderByNumber_timestampMs",
            "rpc_eth_getTransactionByHash_blockTimestampMs",
            "rpc_eth_getTransactionByBlockHashAndIndex_blockTimestampMs",
            "rpc_eth_getTransactionByBlockNumberAndIndex_blockTimestampMs",
            "rpc_eth_getLogs_blockTimestampMs",
            "rpc_eth_getFilterChanges_blockTimestampMs",
            "rpc_eth_getFilterLogs_blockTimestampMs",
            "rpc_eth_getTransactionReceipt_logs_blockTimestampMs",
            "rpc_eth_getBlockReceipts_logs_blockTimestampMs",
        ] {
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
                "canonical metadata timestamp",
                "canonical BaseTime metadata unavailable",
            );
        }
        for name in [
            "rpc_eth_subscribe_newHeads_timestampMs",
            "rpc_eth_subscribe_logs_blockTimestampMs",
            "rpc_eth_subscribe_transactionReceipts_logs_blockTimestampMs",
        ] {
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
                "live event",
                "canonical BaseTime metadata unavailable",
            );
        }
        return;
    };
    let hash = report.block_hash.to_string();
    let number = format!("0x{:x}", report.block_number);
    for (name, method, params) in [
        ("rpc_eth_getBlockByHash_timestampMs", "eth_getBlockByHash", json!([hash, false])),
        ("rpc_eth_getBlockByNumber_timestampMs", "eth_getBlockByNumber", json!([number, false])),
        ("rpc_eth_getHeaderByHash_timestampMs", "eth_getHeaderByHash", json!([hash])),
        ("rpc_eth_getHeaderByNumber_timestampMs", "eth_getHeaderByNumber", json!([number])),
    ] {
        let identity = [("hash", hash.clone()), ("number", number.clone())];
        check_wire_object(
            report,
            name,
            raw.call(method, params).await,
            "timestampMs",
            timestamp_ms,
            &identity,
        );
    }
    let Some(tx) = block.transactions.txns().nth(1) else { return };
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
            raw.call(method, params).await,
            "blockTimestampMs",
            timestamp_ms,
            &identity,
        );
    }
    let filter = json!({"blockHash": hash});
    check_wire_logs(
        report,
        "rpc_eth_getLogs_blockTimestampMs",
        raw.call("eth_getLogs", json!([filter])).await,
        timestamp_ms,
        &hash,
    );
    let filter_id = raw.call("eth_newFilter", json!([filter])).await;
    match filter_id {
        RawOutcome::Value(id) if id.as_str().and_then(parse_quantity).is_some() => {
            check_wire_logs(
                report,
                "rpc_eth_getFilterChanges_blockTimestampMs",
                raw.call("eth_getFilterChanges", json!([id])).await,
                timestamp_ms,
                &hash,
            );
            check_wire_logs(
                report,
                "rpc_eth_getFilterLogs_blockTimestampMs",
                raw.call("eth_getFilterLogs", json!([id])).await,
                timestamp_ms,
                &hash,
            );
            let cleanup = raw.call("eth_uninstallFilter", json!([id])).await;
            if !matches!(cleanup, RawOutcome::Value(Value::Bool(true))) {
                for name in [
                    "rpc_eth_getFilterChanges_blockTimestampMs",
                    "rpc_eth_getFilterLogs_blockTimestampMs",
                ] {
                    if let Some(check) =
                        report.checks.iter_mut().rev().find(|check| check.name == name)
                    {
                        check.status = ZenithCheckStatus::Fail;
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
                    RawOutcome::MethodError(error) => (ZenithCheckStatus::Fail, error.as_str()),
                    RawOutcome::Unavailable(error) => {
                        (ZenithCheckStatus::Indeterminate, error.as_str())
                    }
                    RawOutcome::Value(_) => (ZenithCheckStatus::Fail, "invalid filter identifier"),
                };
                add_observation(report, name, status, "blockHash filter", reason);
            }
        }
    }
    let receipt = raw.call("eth_getTransactionReceipt", json!([tx_hash])).await;
    let receipt_logs = match receipt {
        RawOutcome::Value(value)
            if value.get("transactionHash").and_then(Value::as_str) == Some(tx_hash.as_str())
                && value.get("blockHash").and_then(Value::as_str) == Some(hash.as_str())
                && value.get("blockNumber").and_then(Value::as_str).and_then(parse_quantity)
                    == Some(report.block_number) =>
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
        timestamp_ms,
        &hash,
    );
    let receipts = raw.call("eth_getBlockReceipts", json!([hash])).await;
    let nested = match receipts {
        RawOutcome::Value(value) => match value.as_array() {
            Some(receipts)
                if receipts.iter().all(|receipt| {
                    receipt.get("blockHash").and_then(Value::as_str) == Some(hash.as_str())
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
        timestamp_ms,
        &hash,
    );
    append_subscription_checks(provider, report, el_ws_rpc, target).await;
}

async fn subscription_event_result<P: Provider<Base>>(
    provider: &P,
    kind: &str,
    notification: &Value,
) -> (ZenithCheckStatus, String) {
    let Some(result) = notification.pointer("/params/result") else {
        return (ZenithCheckStatus::Fail, "notification has no result".into());
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
        return (ZenithCheckStatus::Fail, "event has no valid block hash".into());
    };
    let block = match provider.get_block(BlockId::Hash(hash.into())).full().await {
        Ok(Some(block)) if block.header.hash == hash => block,
        _ => return (ZenithCheckStatus::Indeterminate, "event block unavailable".into()),
    };
    let transactions =
        block.transactions.txns().take(2).map(|tx| tx.as_ref().clone()).collect::<Vec<_>>();
    let Some(expected) =
        BaseTimeUpdateTx::extract_from_transactions(&transactions, block.header.number).ok().map(
            |metadata| block.header.timestamp * 1_000 + u64::from(metadata.timestamp_millis_part()),
        )
    else {
        return (ZenithCheckStatus::Indeterminate, "event block metadata unavailable".into());
    };
    if kind == "transactionReceipts" {
        let Some(receipts) = result.as_array().filter(|receipts| !receipts.is_empty()) else {
            return (ZenithCheckStatus::Fail, "malformed receipt event".into());
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
            return (ZenithCheckStatus::Fail, "malformed or mismatched receipt event".into());
        }
        let logs: Vec<_> = receipts
            .iter()
            .filter_map(|receipt| receipt.get("logs").and_then(Value::as_array))
            .flatten()
            .collect();
        if logs.is_empty() {
            return (ZenithCheckStatus::Indeterminate, "receipt event has no logs".into());
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
            if valid { ZenithCheckStatus::Pass } else { ZenithCheckStatus::Fail },
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
        if valid { ZenithCheckStatus::Pass } else { ZenithCheckStatus::Fail },
        if valid {
            format!("matching event timestamp 0x{expected:x}")
        } else {
            "missing or incorrect event timestamp".into()
        },
    )
}

async fn append_subscription_checks<P: Provider<Base>>(
    provider: &P,
    report: &mut ZenithReport,
    el_ws_rpc: Option<&Url>,
    target: ZenithCheckTarget,
) {
    const SUBSCRIPTIONS: [(&str, &str); 3] = [
        ("rpc_eth_subscribe_newHeads_timestampMs", "newHeads"),
        ("rpc_eth_subscribe_logs_blockTimestampMs", "logs"),
        ("rpc_eth_subscribe_transactionReceipts_logs_blockTimestampMs", "transactionReceipts"),
    ];
    let Some(el_ws_rpc) = el_ws_rpc else {
        for (name, _) in SUBSCRIPTIONS {
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
                "matching WebSocket event",
                "standard execution WebSocket RPC is not configured",
            );
        }
        return;
    };
    if matches!(target, ZenithCheckTarget::BlockHash(_)) {
        for (name, _) in SUBSCRIPTIONS {
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
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
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
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
    let mut queued = HashSet::new();
    let mut notifications = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while completed.len() + queued.len() < SUBSCRIPTIONS.len() {
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
                add_observation(
                    report,
                    name,
                    ZenithCheckStatus::Fail,
                    "accepted WebSocket subscription",
                    error.get("message").and_then(Value::as_str).unwrap_or("subscription rejected"),
                );
                completed.insert(name);
            } else if let Some(subscription) = value.get("result").and_then(Value::as_str) {
                subscriptions.insert(subscription.to_string(), (name, kind));
            } else {
                add_observation(
                    report,
                    name,
                    ZenithCheckStatus::Fail,
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
        if completed.contains(name) || queued.contains(name) {
            continue;
        }
        queued.insert(name);
        notifications.push((name, kind, value));
    }
    for (name, kind, value) in notifications {
        let (status, observed) = subscription_event_result(provider, kind, &value).await;
        add_observation(report, name, status, "matching timestamped WebSocket event", observed);
        completed.insert(name);
    }
    for (name, _) in SUBSCRIPTIONS {
        if !completed.contains(name) {
            add_observation(
                report,
                name,
                ZenithCheckStatus::Indeterminate,
                "matching timestamped WebSocket event",
                if pending.values().any(|(pending_name, _)| *pending_name == name) {
                    "subscription response unavailable"
                } else {
                    "subscription accepted; no correlated event observed"
                },
            );
        }
    }
    let _ = socket.close(None).await;
}

impl ZenithReport {
    /// Evaluates complete RPC observations into the stable report model.
    pub fn evaluate(input: ZenithObservations) -> Self {
        let schedule = match input.activation {
            None => ZenithSchedule::NotScheduled,
            Some(at) if input.timestamp < at => ZenithSchedule::Scheduled,
            Some(_) => ZenithSchedule::Active,
        };
        let active = schedule == ZenithSchedule::Active;
        let initial = input.implementation
            == U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice());
        let empty_code_hash = keccak256([]);
        let canonical_admin =
            input.admin == U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice());
        let installation = if input.proxy_code_hash == empty_code_hash {
            ZenithInstallation::Missing
        } else if input.proxy_code_hash != BaseTime::PROXY_CODE_HASH || !canonical_admin {
            ZenithInstallation::Inconsistent
        } else if input.implementation == U256::ZERO {
            ZenithInstallation::Dormant
        } else if !input.implementation_is_address {
            ZenithInstallation::Inconsistent
        } else if initial
            && input.implementation_code_hash == Some(BaseTime::IMPLEMENTATION_CODE_HASH)
        {
            ZenithInstallation::LinkedInitial
        } else if initial
            || input.implementation_code_hash.is_none()
            || input.implementation_code_hash == Some(empty_code_hash)
        {
            ZenithInstallation::Inconsistent
        } else {
            ZenithInstallation::LinkedOther
        };
        let identity_matches = input.el_chain_id == input.cl_chain_id
            && input.expected_chain_id.is_none_or(|expected| expected == input.el_chain_id);
        let expected_ms = input
            .metadata_millis_part
            .map(|part| input.timestamp.wrapping_mul(1_000).wrapping_add(u64::from(part)));
        let metadata_matches = input.metadata_millis_part.is_some()
            && input.metadata_receipt_valid == Some(true)
            && input.timestamp_ms == expected_ms
            && Some(input.storage_millis_part) == input.metadata_millis_part
            && input.getter_millis_part == input.metadata_millis_part
            && input.getter_timestamp_ms == expected_ms;
        let metadata = if active {
            if metadata_matches { ZenithCheckStatus::Pass } else { ZenithCheckStatus::Fail }
        } else {
            ZenithCheckStatus::Indeterminate
        };
        let mut checks = Vec::new();
        let identity_endpoint = format!("{}, {}", input.el_endpoint, input.cl_endpoint);
        let context = ZenithCheckContext {
            endpoint: &input.el_endpoint,
            block_number: input.block_number,
            block_hash: input.block_hash,
        };
        ZenithCheck::push(
            &mut checks,
            ZenithCheckContext { endpoint: &identity_endpoint, ..context },
            "chain_id",
            (identity_matches, true),
            (
                input.expected_chain_id.unwrap_or(input.cl_chain_id),
                format!("el={}, cl={}", input.el_chain_id, input.cl_chain_id),
            ),
            "point basectl, EL, and CL at the same L2 chain",
        );
        ZenithCheck::push(
            &mut checks,
            context,
            "el_syncing",
            (!input.el_syncing, true),
            (false, input.el_syncing),
            "wait for the EL to finish syncing before evaluating activation",
        );
        ZenithCheck::push(
            &mut checks,
            context,
            "snapshot_consistency",
            (input.snapshot_consistent, true),
            (
                "matching number/hash block, header, and transaction views",
                input.snapshot_consistent,
            ),
            "use a consistent archive-capable EL RPC",
        );
        for (name, matches, expected, observed, remediation) in [
            (
                "proxy_code_hash",
                input.proxy_code_hash == BaseTime::PROXY_CODE_HASH,
                BaseTime::PROXY_CODE_HASH.to_string(),
                input.proxy_code_hash.to_string(),
                "install the canonical BaseTime proxy",
            ),
            (
                "proxy_admin",
                canonical_admin,
                Predeploys::PROXY_ADMIN.to_string(),
                input.admin.to_string(),
                "restore the canonical EIP-1967 proxy admin",
            ),
            (
                "implementation",
                matches!(
                    installation,
                    ZenithInstallation::LinkedInitial | ZenithInstallation::LinkedOther
                ),
                "a linked implementation with deployed code".into(),
                format!("{installation:?}"),
                "link a valid BaseTime implementation",
            ),
        ] {
            ZenithCheck::push(
                &mut checks,
                context,
                name,
                (matches, active),
                (expected, observed),
                remediation,
            );
        }
        if active {
            for (name, matches, expected, observed, remediation) in [
                (
                    "metadata",
                    input.metadata_millis_part.is_some(),
                    "canonical tx[1] BaseTime deposit".into(),
                    input.metadata_error.clone().unwrap_or_else(|| {
                        input
                            .metadata_millis_part
                            .map_or_else(|| "missing".into(), |part| part.to_string())
                    }),
                    "include the canonical BaseTime metadata deposit at tx[1]",
                ),
                (
                    "metadata_receipt",
                    input.metadata_receipt_valid == Some(true),
                    "successful receipt at index 1 in the snapshot block".into(),
                    input
                        .metadata_receipt_valid
                        .map_or_else(|| "missing".into(), |valid| valid.to_string()),
                    "inspect the BaseTime metadata deposit execution",
                ),
                (
                    "header_timestamp_ms",
                    matches!(
                        (input.timestamp_ms, expected_ms),
                        (Some(observed), Some(expected)) if observed == expected
                    ),
                    expected_ms.map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.timestamp_ms.map_or_else(|| "missing".into(), |v| v.to_string()),
                    "upgrade the EL RPC and verify header timestampMs",
                ),
                (
                    "storage_millis_part",
                    Some(input.storage_millis_part) == input.metadata_millis_part,
                    input
                        .metadata_millis_part
                        .map_or_else(|| "metadata unavailable".into(), |v| v.to_string()),
                    input.storage_millis_part.to_string(),
                    "verify BaseTime state transition execution",
                ),
                (
                    "getter_millis_part",
                    matches!(
                        (input.getter_millis_part, input.metadata_millis_part),
                        (Some(observed), Some(expected)) if observed == expected
                    ),
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
                    "verify BaseTime proxy linkage and getter execution",
                ),
                (
                    "getter_timestamp_ms",
                    matches!(
                        (input.getter_timestamp_ms, expected_ms),
                        (Some(observed), Some(expected)) if observed == expected
                    ),
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
                    "verify BaseTime proxy linkage and timestampMs getter",
                ),
            ] {
                ZenithCheck::push(
                    &mut checks,
                    context,
                    name,
                    (matches, true),
                    (expected, observed),
                    remediation,
                );
            }
        }
        let overall = if checks.iter().any(|check| check.status == ZenithCheckStatus::Fail) {
            ZenithStatus::Broken
        } else {
            ZenithStatus::Healthy
        };
        Self {
            overall,
            identity: if identity_matches {
                ZenithCheckStatus::Pass
            } else {
                ZenithCheckStatus::Fail
            },
            schedule,
            installation,
            metadata,
            rpc_http: if input.getter_millis_part_error.is_some()
                || input.getter_timestamp_ms_error.is_some()
            {
                ZenithRpcStatus::Fail
            } else {
                ZenithRpcStatus::Pass
            },
            chain_id: input.el_chain_id,
            block_number: input.block_number,
            block_hash: input.block_hash,
            timestamp: input.timestamp,
            timestamp_ms: input.timestamp_ms,
            activation: input.activation,
            checks,
            cursor: None,
        }
    }
}

/// Snapshot and endpoint context shared by evaluated checks.
#[derive(Debug, Clone, Copy)]
pub struct ZenithCheckContext<'a> {
    /// RPC endpoint responsible for the observation.
    pub endpoint: &'a str,
    /// Snapshot block number.
    pub block_number: u64,
    /// Snapshot block hash.
    pub block_hash: B256,
}

impl ZenithCheck {
    /// Appends an evaluated invariant with its snapshot context.
    pub fn push(
        checks: &mut Vec<Self>,
        context: ZenithCheckContext<'_>,
        name: &str,
        evaluation: (bool, bool),
        values: (impl ToString, impl ToString),
        remediation: &str,
    ) {
        let (matches, required) = evaluation;
        let (expected, observed) = values;
        checks.push(Self {
            name: name.into(),
            status: if matches {
                ZenithCheckStatus::Pass
            } else if required {
                ZenithCheckStatus::Fail
            } else {
                ZenithCheckStatus::Indeterminate
            },
            endpoint: context.endpoint.into(),
            block_number: context.block_number,
            block_hash: context.block_hash,
            expected: expected.to_string(),
            observed: observed.to_string(),
            remediation: remediation.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{B256, U256};
    use axum::{Json, Router, extract::State, routing::post};
    use base_common_consensus::Predeploys;
    use base_common_evm::BaseTime;
    use base_common_genesis::RollupConfig;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use url::Url;

    use super::*;

    async fn zenith_rpc_fixture(
        State(requests): State<Arc<Mutex<Vec<Value>>>>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        requests.lock().unwrap().push(request.clone());
        let hash = B256::repeat_byte(0x42);
        let result = match request["method"].as_str().unwrap() {
            "optimism_rollupConfig" => {
                let mut config = serde_json::to_value(RollupConfig::default()).unwrap();
                config["l2_chain_id"] = json!(8453);
                config["base"]["zenith"] = json!(20);
                config
            }
            "eth_chainId" => json!("0x2105"),
            "eth_syncing" => json!(false),
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

    fn observations(activation: Option<u64>, timestamp: u64) -> ZenithObservations {
        let part = 200;
        ZenithObservations {
            el_endpoint: "http://el".into(),
            cl_endpoint: "http://cl".into(),
            el_chain_id: 8453,
            cl_chain_id: 8453,
            expected_chain_id: Some(8453),
            activation,
            el_syncing: false,
            block_number: 10,
            block_hash: B256::ZERO,
            timestamp,
            timestamp_ms: Some(timestamp.wrapping_mul(1_000).wrapping_add(u64::from(part))),
            snapshot_consistent: true,
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
            Router::new().route("/", post(zenith_rpc_fixture)).with_state(Arc::clone(&requests));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let url = Url::parse(&format!("http://{address}")).unwrap();
        let hash = B256::repeat_byte(0x42);
        let report = ZenithChecker::new(Some(8453))
            .check(&url, &url, ZenithCheckTarget::BlockHash(hash))
            .await
            .unwrap();

        assert_eq!(report.overall, ZenithStatus::Healthy);
        assert_eq!(report.schedule, ZenithSchedule::Scheduled);
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
        assert_eq!(hash_reads.len(), 3);
        assert_eq!(
            hash_reads.iter().filter(|request| request["params"][0] == json!(hash)).count(),
            3
        );
        let number_reads: Vec<_> = requests
            .iter()
            .filter(|request| {
                matches!(
                    request["method"].as_str(),
                    Some("eth_getBlockByNumber" | "eth_getHeaderByNumber")
                )
            })
            .collect();
        assert!(number_reads.iter().filter(|request| request["params"][0] == "0xa").count() >= 2);
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
        let unscheduled = ZenithReport::evaluate(observations(None, 10));
        assert_eq!(unscheduled.schedule, ZenithSchedule::NotScheduled);
        assert_eq!(unscheduled.metadata, ZenithCheckStatus::Indeterminate);
        assert_eq!(unscheduled.overall, ZenithStatus::Healthy);

        let scheduled = ZenithReport::evaluate(observations(Some(20), 10));
        assert_eq!(scheduled.schedule, ZenithSchedule::Scheduled);
        assert_eq!(scheduled.overall, ZenithStatus::Healthy);
    }

    #[test]
    fn active_snapshot_requires_canonical_metadata_and_state() {
        let mut input = observations(Some(10), 10);
        input.storage_millis_part = 400;
        let report = ZenithReport::evaluate(input);
        assert_eq!(report.schedule, ZenithSchedule::Active);
        assert_eq!(report.metadata, ZenithCheckStatus::Fail);
        assert_eq!(report.overall, ZenithStatus::Broken);
    }

    #[test]
    fn configured_chain_identity_mismatch_fails() {
        let mut input = observations(None, 10);
        input.expected_chain_id = Some(84532);
        let report = ZenithReport::evaluate(input);
        assert_eq!(report.identity, ZenithCheckStatus::Fail);
        assert_eq!(report.overall, ZenithStatus::Broken);
    }

    #[test]
    fn snapshot_inconsistency_fails_its_named_check() {
        let mut input = observations(None, 10);
        input.snapshot_consistent = false;
        let report = ZenithReport::evaluate(input);
        let check =
            report.checks.iter().find(|check| check.name == "snapshot_consistency").unwrap();

        assert_eq!(check.status, ZenithCheckStatus::Fail);
        assert_eq!(report.overall, ZenithStatus::Broken);
    }

    #[test]
    fn governance_changed_implementation_with_code_is_permitted() {
        let mut input = observations(Some(10), 10);
        input.implementation = U256::from(1);
        input.implementation_code_hash = Some(B256::with_last_byte(1));
        let report = ZenithReport::evaluate(input);
        assert_eq!(report.installation, ZenithInstallation::LinkedOther);
        assert_eq!(report.overall, ZenithStatus::Healthy);
    }

    #[test]
    fn malformed_or_empty_implementation_is_inconsistent() {
        let mut malformed = observations(Some(10), 10);
        malformed.implementation_is_address = false;
        assert_eq!(
            ZenithReport::evaluate(malformed).installation,
            ZenithInstallation::Inconsistent
        );

        let mut empty = observations(Some(10), 10);
        empty.implementation = U256::from(1);
        empty.implementation_code_hash = Some(keccak256([]));
        assert_eq!(ZenithReport::evaluate(empty).installation, ZenithInstallation::Inconsistent);

        let mut unavailable = observations(Some(10), 10);
        unavailable.implementation = U256::from(1);
        unavailable.implementation_code_hash = None;
        assert_eq!(
            ZenithReport::evaluate(unavailable).installation,
            ZenithInstallation::Inconsistent
        );
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
        let report = ZenithReport::evaluate(input);

        assert_eq!(report.rpc_http, ZenithRpcStatus::Fail);
        let getter_checks: Vec<_> =
            report.checks.iter().filter(|check| check.name.starts_with("getter_")).collect();
        assert_eq!(getter_checks.len(), 2);
        assert!(getter_checks.iter().all(|check| check.status == ZenithCheckStatus::Fail));
    }

    #[test]
    fn timestamp_comparison_uses_uint64_wrapping_semantics() {
        let timestamp = u64::MAX / 1_000 + 1;
        let mut input = observations(Some(0), timestamp);
        let expected = timestamp.wrapping_mul(1_000).wrapping_add(200);
        input.timestamp_ms = Some(expected);
        input.getter_timestamp_ms = Some(expected);

        assert_eq!(ZenithReport::evaluate(input).metadata, ZenithCheckStatus::Pass);
    }

    #[test]
    fn report_serializes_consumer_dimensions_and_failure_context() {
        let mut input = observations(Some(10), 10);
        input.metadata_receipt_valid = Some(false);
        let value = serde_json::to_value(ZenithReport::evaluate(input)).unwrap();
        assert_eq!(value["overall"], "broken");
        assert_eq!(value["identity"], "pass");
        assert_eq!(value["schedule"], "active");
        assert_eq!(value["installation"], "linked_initial");
        assert_eq!(value["metadata"], "fail");
        assert!(value["checks"][0]["endpoint"].is_string());
        assert!(value["checks"][0]["blockHash"].is_string());
    }

    #[test]
    fn cadence_classifies_boundary_exact_gap_and_wrong_gap() {
        assert_eq!(cadence_status(None, Some(1_000), true), ZenithCheckStatus::Indeterminate);
        assert_eq!(cadence_status(Some(1_000), Some(1_200), true), ZenithCheckStatus::Pass);
        assert_eq!(cadence_status(Some(1_000), Some(1_400), true), ZenithCheckStatus::Fail);
        assert_eq!(
            cadence_status(Some(1_000), Some(1_200), false),
            ZenithCheckStatus::Indeterminate
        );
    }

    #[test]
    fn raw_object_classifies_field_and_method_evidence() {
        let mut report = ZenithReport::evaluate(observations(Some(10), 10));
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
                ZenithCheckStatus::Indeterminate,
                ZenithCheckStatus::Fail,
                ZenithCheckStatus::Fail,
                ZenithCheckStatus::Pass,
            ]
        );
    }
}
