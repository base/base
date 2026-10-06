//! Read-only HTTP JSON API serving persisted shadow blocks to the explorer UI.

use std::collections::HashMap;

use alloy_consensus::{Transaction, TxReceipt, Typed2718};
use alloy_primitives::{B256, hex};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base_common_consensus::BaseTxEnvelope;
use base_shadow_indexer_db::{ShadowBlockRepo, ShadowBlockRow, ShadowHash, ShadowSummaryRow};
use serde::{Deserialize, Serialize};

use crate::{ShadowBlockStats, ShadowMetricsStore};

/// Shared state for the block API handlers.
#[derive(Clone)]
struct ApiState {
    repo: Option<ShadowBlockRepo>,
}

impl ApiState {
    /// Returns the repository handle, or [`ApiError::DbDisabled`] when Postgres is not configured.
    fn repo(&self) -> Result<&ShadowBlockRepo, ApiError> {
        self.repo.as_ref().ok_or(ApiError::DbDisabled)
    }
}

/// Builds the block explorer API router. Consumed server-to-server over the mesh
/// (no browser origin), so it carries no CORS layer.
pub fn api_router(store: Option<ShadowMetricsStore>) -> Router {
    let repo = store.map(|store| ShadowBlockRepo::new(store.pool().clone()));
    Router::new()
        .route("/shadow-candidates", get(get_shadow_candidates_batch))
        .route("/shadow-blocks", get(get_recent_shadow_blocks))
        .route("/blocks/{id}", get(get_block))
        .route("/blocks/{id}/shadow-candidates", get(get_shadow_candidates))
        .route("/shadow-blocks/{id}", get(get_shadow_block))
        .with_state(ApiState { repo })
}

/// One reorged-out shadow block summary.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShadowBlockSummary {
    number: i64,
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_hash: Option<String>,
    timestamp: u64,
    builder_version: String,
    gas_used: u64,
    tx_count: usize,
    non_deposit_tx_count: usize,
    priority_fee_inversions: usize,
}

/// Block overview plus its transaction summaries.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockDetail {
    number: i64,
    hash: String,
    parent_hash: String,
    timestamp: u64,
    gas_used: u64,
    gas_limit: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_fee_per_gas: Option<u64>,
    reorged_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_hash: Option<String>,
    tx_count: usize,
    transactions: Vec<TxSummary>,
}

/// A transaction row within a block.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TxSummary {
    index: usize,
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_used: Option<u64>,
    gas_limit: u64,
    tx_type: String,
}

/// Errors surfaced by the block API.
#[derive(Debug)]
enum ApiError {
    DbDisabled,
    BadRequest,
    NotFound,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::DbDisabled => {
                (StatusCode::SERVICE_UNAVAILABLE, "postgres is not configured\n").into_response()
            }
            Self::BadRequest => (StatusCode::BAD_REQUEST, "invalid block hash\n").into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found\n").into_response(),
            Self::Internal(error) => {
                tracing::error!(error = %error, "block api request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
            }
        }
    }
}

async fn get_block(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<BlockDetail>, ApiError> {
    let row = resolve_block(state.repo()?, &id).await?;
    tracing::info!(
        target: "shadow_metrics::api",
        endpoint = "blocks",
        id = %id,
        number = row.number,
        "served block detail"
    );
    Ok(Json(block_detail(&row)))
}

/// Reorged-out shadow blocks replaced by the canonical block addressed by the path.
async fn get_shadow_candidates(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ShadowBlockSummary>>, ApiError> {
    let repo = state.repo()?;
    let canonical_hash = parse_block_id(&id)?;

    let shadows = repo.list_reorged_by_canonical(&canonical_hash).await?;
    tracing::info!(
        target: "shadow_metrics::api",
        endpoint = "shadow-candidates",
        canonical_hash = %canonical_hash,
        count = shadows.len(),
        "served shadow candidates by canonical hash"
    );
    Ok(Json(shadows.iter().map(shadow_block_summary).collect::<Vec<_>>()))
}

#[derive(Debug, Deserialize)]
struct ShadowCandidatesQuery {
    canonical: Option<String>,
}

/// Batch lookup for reorged-out shadow blocks replaced by canonical hashes.
async fn get_shadow_candidates_batch(
    State(state): State<ApiState>,
    Query(query): Query<ShadowCandidatesQuery>,
) -> Result<Json<HashMap<String, Vec<ShadowBlockSummary>>>, ApiError> {
    let Some(canonical) = query.canonical else {
        return Ok(Json(HashMap::new()));
    };

    let mut parsed = Vec::new();
    for entry in canonical.split(',') {
        if parsed.len() >= 200 {
            break;
        }
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(hash) = parse_block_id(trimmed) {
            parsed.push(hash);
        }
    }

    if parsed.is_empty() {
        return Ok(Json(HashMap::new()));
    }

    let hashes: Vec<String> = parsed;
    let repo = state.repo()?;
    let shadows = repo.list_reorged_by_canonicals(&hashes).await?;

    let mut result: HashMap<String, Vec<ShadowBlockSummary>> = HashMap::new();
    for shadow in &shadows {
        let Some(canonical_hash) = shadow.canonical_hash.as_ref() else {
            continue;
        };
        let key = canonical_hash.clone();
        result.entry(key).or_default().push(shadow_block_summary(shadow));
    }

    tracing::info!(
        target: "shadow_metrics::api",
        endpoint = "shadow-candidates-batch",
        requested = hashes.len(),
        rows = shadows.len(),
        groups = result.len(),
        "served batch shadow candidates"
    );
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct RecentShadowBlocksQuery {
    limit: Option<i64>,
    before: Option<i64>,
}

const DEFAULT_RECENT_LIMIT: i64 = 25;
const MAX_RECENT_LIMIT: i64 = 1000;

/// Absent falls back to the default; the result is clamped to [1, MAX] so a
/// malformed low value cannot make Postgres reject the query and an oversized
/// value cannot force an unbounded scan.
fn resolve_recent_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_RECENT_LIMIT).clamp(1, MAX_RECENT_LIMIT)
}

/// The most recent resolved shadow blocks, newest first, paged backwards by `before`.
async fn get_recent_shadow_blocks(
    State(state): State<ApiState>,
    Query(query): Query<RecentShadowBlocksQuery>,
) -> Result<Json<Vec<ShadowBlockSummary>>, ApiError> {
    let limit = resolve_recent_limit(query.limit);

    let repo = state.repo()?;
    let shadows = repo.list_recent(limit, query.before).await?;
    tracing::info!(
        target: "shadow_metrics::api",
        endpoint = "recent-shadow-blocks",
        limit,
        before = query.before,
        count = shadows.len(),
        "served recent shadow blocks"
    );
    Ok(Json(shadows.iter().map(shadow_block_summary).collect::<Vec<_>>()))
}

/// A single reorged-out shadow block summary addressed by shadow block hash.
async fn get_shadow_block(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<ShadowBlockSummary>, ApiError> {
    let hash = parse_block_id(&id)?;

    let repo = state.repo()?;
    let Some(row) = repo.get_summary_by_block_hash(&hash).await? else {
        tracing::info!(
            target: "shadow_metrics::api",
            endpoint = "shadow-blocks",
            hash = %hash,
            found = false,
            "shadow block not found"
        );
        return Err(ApiError::NotFound);
    };

    tracing::info!(
        target: "shadow_metrics::api",
        endpoint = "shadow-blocks",
        hash = %hash,
        found = true,
        "served shadow block"
    );
    Ok(Json(shadow_block_summary(&row)))
}

/// Parses a path segment as a block hash, normalized to the spelling the table stores.
///
/// Lookups are string equality against `shadow_blocks.hash`, so `0xAB..` from a caller must not
/// miss a row written as `0xab..`. Going through `B256` both rejects anything that is not a hash
/// and collapses every accepted spelling onto the one [`ShadowHash`] writes.
fn parse_block_id(id: &str) -> Result<String, ApiError> {
    id.trim()
        .parse::<B256>()
        .map(|hash| ShadowHash::encode(hash.as_slice()))
        .map_err(|_| ApiError::BadRequest)
}

/// Resolves a stored block by hash (canonical or reorged-out shadow).
async fn resolve_block(repo: &ShadowBlockRepo, id: &str) -> Result<ShadowBlockRow, ApiError> {
    let hash = parse_block_id(id)?;
    repo.get_by_block_hash(&hash).await?.ok_or(ApiError::NotFound)
}

fn shadow_block_summary(row: &ShadowSummaryRow) -> ShadowBlockSummary {
    let stats = ShadowBlockStats::from_parts(
        row.number,
        row.builder_version.clone(),
        &row.header.0,
        &row.transactions.0,
    );

    ShadowBlockSummary {
        number: row.number,
        hash: row.hash.clone(),
        canonical_hash: row.canonical_hash.clone(),
        timestamp: row.header.0.timestamp,
        builder_version: stats.builder_version,
        gas_used: stats.gas_used,
        tx_count: stats.transaction_count,
        non_deposit_tx_count: stats.non_deposit_tx_count,
        priority_fee_inversions: stats.priority_fee_inversions,
    }
}

fn block_detail(row: &ShadowBlockRow) -> BlockDetail {
    let block = &row.payload.block;
    let header = block.header();
    let gas_used = per_tx_gas_used(row);
    let transactions = block
        .body()
        .transactions
        .iter()
        .enumerate()
        .map(|(index, tx)| tx_summary(row, index, tx, gas_used.get(index).copied().flatten()))
        .collect();

    BlockDetail {
        number: row.number,
        hash: row.hash.clone(),
        parent_hash: hex::encode_prefixed(header.parent_hash),
        timestamp: header.timestamp,
        gas_used: header.gas_used,
        gas_limit: header.gas_limit,
        base_fee_per_gas: header.base_fee_per_gas,
        // Constant so the response shape survives the column drop: the table only ever holds
        // blocks the chain discarded.
        reorged_out: true,
        canonical_hash: row.canonical_hash.clone(),
        tx_count: block.body().transactions.len(),
        transactions,
    }
}

fn tx_summary(
    row: &ShadowBlockRow,
    index: usize,
    tx: &BaseTxEnvelope,
    gas_used: Option<u64>,
) -> TxSummary {
    TxSummary {
        index,
        hash: hex::encode_prefixed(tx.tx_hash()),
        from: sender_at(row, index),
        to: tx.to().map(hex::encode_prefixed),
        gas_used,
        gas_limit: tx.gas_limit(),
        tx_type: tx_type_str(tx).to_owned(),
    }
}

/// Recovered sender address at a transaction index, if senders are present.
fn sender_at(row: &ShadowBlockRow, index: usize) -> Option<String> {
    row.payload.block.senders().get(index).map(hex::encode_prefixed)
}

/// Per-transaction gas derived from receipt cumulative deltas.
///
/// Returns `None` per index when the receipt vector length does not match the transactions.
fn per_tx_gas_used(row: &ShadowBlockRow) -> Vec<Option<u64>> {
    let tx_count = row.payload.block.body().transactions.len();
    if row.payload.receipts.len() != tx_count {
        return vec![None; tx_count];
    }

    let mut previous = 0u64;
    row.payload
        .receipts
        .iter()
        .map(|receipt| {
            let cumulative = receipt.cumulative_gas_used();
            let used = cumulative.saturating_sub(previous);
            previous = cumulative;
            Some(used)
        })
        .collect()
}

fn tx_type_str(tx: &BaseTxEnvelope) -> &'static str {
    if tx.is_deposit() {
        "deposit"
    } else if tx.is_eip8130() {
        "eip8130"
    } else if tx.is_eip1559() {
        "eip1559"
    } else if tx.is_eip2930() {
        "eip2930"
    } else if tx.is_eip7702() {
        "eip7702"
    } else {
        "legacy"
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Block, BlockBody, Header, Receipt, Sealable};
    use alloy_primitives::{Address, TxKind, U256};
    use base_common_consensus::{BaseReceipt, TxDeposit};
    use base_shadow_indexer_db::ShadowBlockPayload;
    use chrono::Utc;
    use reth_primitives_traits::RecoveredBlock;
    use sqlx::types::Json;

    use super::*;

    const SENDER: Address = Address::repeat_byte(0x22);
    const RECIPIENT: Address = Address::repeat_byte(0x11);

    fn sample_row() -> ShadowBlockRow {
        sample_row_with(None)
    }

    fn sample_row_with(canonical_hash: Option<String>) -> ShadowBlockRow {
        sample_row_full(42, 21_000, "test", canonical_hash)
    }

    fn sample_row_full(
        number: i64,
        gas_used: u64,
        builder_version: &str,
        canonical_hash: Option<String>,
    ) -> ShadowBlockRow {
        let deposit = TxDeposit {
            gas_limit: 21_000,
            to: TxKind::Call(RECIPIENT),
            value: U256::from(7u64),
            ..Default::default()
        };
        let env = BaseTxEnvelope::Deposit(deposit.seal_slow());
        let body = BlockBody { transactions: vec![env], ommers: vec![], withdrawals: None };
        let block: base_common_consensus::BaseBlock =
            Block { header: Header { gas_used, ..Default::default() }, body };
        let recovered = RecoveredBlock::new_unhashed(block, vec![SENDER]);

        let receipts = vec![BaseReceipt::Eip1559(Receipt {
            status: true.into(),
            cumulative_gas_used: gas_used,
            logs: Vec::new(),
        })];

        let now = Utc::now();
        ShadowBlockRow {
            number,
            hash: ShadowHash::encode(&[0xab; 32]),
            canonical_hash,
            created_at: now,
            updated_at: now,
            payload: ShadowBlockPayload {
                builder_version: builder_version.to_owned(),
                block: recovered,
                receipts,
            },
        }
    }

    #[test]
    fn block_detail_derives_per_tx_gas_and_sender() {
        let detail = block_detail(&sample_row());
        assert_eq!(detail.number, 42);
        assert_eq!(detail.gas_used, 21_000);
        assert_eq!(detail.tx_count, 1);

        let tx = &detail.transactions[0];
        assert_eq!(tx.index, 0);
        assert_eq!(tx.gas_used, Some(21_000));
        assert_eq!(tx.gas_limit, 21_000);
        assert_eq!(tx.tx_type, "deposit");
        assert_eq!(tx.from.as_deref(), Some(hex::encode_prefixed(SENDER).as_str()));
        assert_eq!(tx.to.as_deref(), Some(hex::encode_prefixed(RECIPIENT).as_str()));
    }

    #[test]
    fn per_tx_gas_used_falls_back_when_receipts_mismatch() {
        let mut row = sample_row();
        assert_eq!(per_tx_gas_used(&row), vec![Some(21_000)]);

        row.payload.receipts.clear();
        assert_eq!(per_tx_gas_used(&row), vec![None]);
    }

    #[test]
    fn block_detail_omits_canonical_hash_when_unresolved() {
        let detail = block_detail(&sample_row());
        assert!(detail.canonical_hash.is_none());
    }

    #[test]
    fn block_detail_exposes_replacement_hash() {
        let detail = block_detail(&sample_row_with(Some(ShadowHash::encode(&[0xcd; 32]))));
        assert_eq!(
            detail.canonical_hash.as_deref(),
            Some(format!("0x{}", "cd".repeat(32)).as_str())
        );
    }

    #[test]
    fn parse_block_id_accepts_hash_and_rejects_non_hash() {
        let hash = format!("0x{}", "ab".repeat(32));
        assert!(parse_block_id(&hash).is_ok());
        assert!(parse_block_id(&format!("  {hash} ")).is_ok());

        assert!(matches!(parse_block_id("42"), Err(ApiError::BadRequest)));
        assert!(matches!(parse_block_id("nothex"), Err(ApiError::BadRequest)));
        assert!(matches!(parse_block_id("0x1234"), Err(ApiError::BadRequest)));
    }

    fn sample_summary_row(row: &ShadowBlockRow) -> ShadowSummaryRow {
        ShadowSummaryRow {
            number: row.number,
            hash: row.hash.clone(),
            canonical_hash: row.canonical_hash.clone(),
            builder_version: row.payload.builder_version.clone(),
            header: Json(row.payload.block.header().clone()),
            transactions: Json(row.payload.block.body().transactions.clone()),
        }
    }

    #[test]
    fn resolve_recent_limit_defaults_and_clamps() {
        assert_eq!(resolve_recent_limit(None), DEFAULT_RECENT_LIMIT);
        assert_eq!(resolve_recent_limit(Some(50)), 50);
        assert_eq!(resolve_recent_limit(Some(0)), 1);
        assert_eq!(resolve_recent_limit(Some(-5)), 1);
        assert_eq!(resolve_recent_limit(Some(MAX_RECENT_LIMIT + 1)), MAX_RECENT_LIMIT);
    }

    #[test]
    fn shadow_block_summary_reports_shadow_only_fields() {
        let shadow = sample_row_full(100, 30_000, "shadow", Some(ShadowHash::encode(&[0xcd; 32])));
        let summary = shadow_block_summary(&sample_summary_row(&shadow));

        assert_eq!(summary.number, 100);
        assert_eq!(
            summary.canonical_hash.as_deref(),
            Some(format!("0x{}", "cd".repeat(32)).as_str())
        );
        assert_eq!(summary.gas_used, 30_000);
        assert_eq!(summary.tx_count, 1);
        assert_eq!(summary.builder_version, "shadow");
    }
}
