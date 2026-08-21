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
use base_shadow_indexer_db::{ShadowBlockRepo, ShadowBlockRow};
use serde::{Deserialize, Serialize};

use crate::{ShadowBlockStats, ShadowMetricsStore};

/// Shared state for the block API handlers.
#[derive(Clone)]
struct ApiState {
    store: Option<ShadowMetricsStore>,
}

impl ApiState {
    /// Returns a repository handle, or [`ApiError::DbDisabled`] when Postgres is not configured.
    fn repo(&self) -> Result<ShadowBlockRepo, ApiError> {
        let store = self.store.as_ref().ok_or(ApiError::DbDisabled)?;
        Ok(ShadowBlockRepo::new(store.pool().clone()))
    }
}

/// Builds the block explorer API router. Consumed server-to-server over the mesh
/// (no browser origin), so it carries no CORS layer.
pub fn api_router(store: Option<ShadowMetricsStore>) -> Router {
    Router::new()
        .route("/shadow-candidates", get(get_shadow_candidates_batch))
        .route("/blocks/{id}", get(get_block))
        .route("/blocks/{id}/shadow-candidates", get(get_shadow_candidates))
        .route("/shadow-blocks/{id}", get(get_shadow_block))
        .with_state(ApiState { store })
}

/// One reorged-out shadow block paired with the canonical block that replaced it.
///
/// The `*_diff` fields are `shadow - canonical`, so a positive value means the shadow
/// block used more. Canonical-derived fields are `None` when the replacement row is not
/// found (for example, still pending persistence).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShadowBlockSummary {
    number: i64,
    hash: String,
    canonical_hash: String,
    timestamp: u64,
    shadow_builder_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_builder_version: Option<String>,
    shadow_gas_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_gas_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_diff_abs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_diff_pct: Option<f64>,
    shadow_tx_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_tx_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_count_diff: Option<i64>,
    shadow_non_deposit_tx_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_non_deposit_tx_count: Option<usize>,
    shadow_priority_fee_inversions: usize,
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
    let row = resolve_block(&state.repo()?, &id).await?;
    Ok(Json(block_detail(&row)))
}

/// Reorged-out shadow blocks replaced by the canonical block addressed by the path.
async fn get_shadow_candidates(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ShadowBlockSummary>>, ApiError> {
    let repo = state.repo()?;
    let canonical_hash = match parse_block_id(&id)? {
        BlockId::Number(number) => match repo.get_canonical_by_number(number).await? {
            Some(row) => row.hash,
            None => return Ok(Json(Vec::new())),
        },
        BlockId::Hash(hash) => hash.to_vec(),
    };

    let shadows = repo.list_reorged_by_canonical(canonical_hash.as_slice()).await?;
    let canonical = repo.get_by_block_hash(canonical_hash.as_slice()).await?;

    let blocks =
        shadows.iter().map(|shadow| shadow_block_summary(shadow, canonical.as_ref())).collect();

    Ok(Json(blocks))
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
        match parse_block_id(trimmed) {
            Ok(BlockId::Hash(hash)) => parsed.push(hash),
            Ok(BlockId::Number(_)) | Err(_) => continue,
        }
    }

    if parsed.is_empty() {
        return Ok(Json(HashMap::new()));
    }

    let hashes: Vec<Vec<u8>> = parsed.into_iter().map(|hash| hash.to_vec()).collect();
    let repo = state.repo()?;
    let shadows = repo.list_reorged_by_canonicals(&hashes).await?;
    let canonicals = repo.list_canonical_by_hashes(&hashes).await?;
    let canonical_by_hash: HashMap<&[u8], &ShadowBlockRow> =
        canonicals.iter().map(|row| (row.hash.as_slice(), row)).collect();

    let mut result: HashMap<String, Vec<ShadowBlockSummary>> = HashMap::new();
    for shadow in &shadows {
        let Some(canonical_hash) = shadow.canonical_hash.as_ref() else {
            continue;
        };
        let key = hex::encode_prefixed(canonical_hash.as_slice());
        let canonical = canonical_by_hash.get(canonical_hash.as_slice()).copied();
        result.entry(key).or_default().push(shadow_block_summary(shadow, canonical));
    }

    Ok(Json(result))
}

/// A single reorged-out shadow block paired with its canonical replacement,
/// including the health verdict. Addressed by shadow block hash.
async fn get_shadow_block(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<ShadowBlockSummary>, ApiError> {
    let BlockId::Hash(hash) = parse_block_id(&id)? else {
        return Err(ApiError::BadRequest);
    };

    let repo = state.repo()?;
    let row = repo.get_by_block_hash(hash.as_slice()).await?.ok_or(ApiError::NotFound)?;
    if !row.reorged_out || row.canonical_hash.is_none() {
        return Err(ApiError::NotFound);
    }

    let canonical = match row.canonical_hash.as_ref() {
        Some(hash) => repo.get_by_block_hash(hash).await?,
        None => None,
    };

    Ok(Json(shadow_block_summary(&row, canonical.as_ref())))
}

/// A block identifier accepted in the path: a decimal block number or a block hash.
enum BlockId {
    Number(i64),
    Hash(B256),
}

/// Parses a path segment as a decimal block number, else a `0x`-hex block hash.
fn parse_block_id(id: &str) -> Result<BlockId, ApiError> {
    let id = id.trim();
    if let Ok(number) = id.parse::<i64>() {
        return Ok(BlockId::Number(number));
    }
    id.parse::<B256>().map(BlockId::Hash).map_err(|_| ApiError::BadRequest)
}

/// Resolves a block by number (canonical) or by hash (canonical or reorged-out shadow).
async fn resolve_block(repo: &ShadowBlockRepo, id: &str) -> Result<ShadowBlockRow, ApiError> {
    let row = match parse_block_id(id)? {
        BlockId::Number(number) => repo.get_canonical_by_number(number).await?,
        BlockId::Hash(hash) => repo.get_by_block_hash(hash.as_slice()).await?,
    };
    row.ok_or(ApiError::NotFound)
}

fn shadow_block_summary(
    row: &ShadowBlockRow,
    canonical: Option<&ShadowBlockRow>,
) -> ShadowBlockSummary {
    let shadow = ShadowBlockStats::from_row(row);
    let canonical = canonical.map(ShadowBlockStats::from_row);

    let gas_diff_abs = canonical.as_ref().map(|c| shadow.gas_used as i64 - c.gas_used as i64);
    let gas_diff_pct = canonical.as_ref().and_then(|c| {
        (c.gas_used != 0)
            .then(|| (shadow.gas_used as f64 - c.gas_used as f64) / c.gas_used as f64 * 100.0)
    });
    let tx_count_diff =
        canonical.as_ref().map(|c| shadow.transaction_count as i64 - c.transaction_count as i64);

    ShadowBlockSummary {
        number: row.number,
        hash: hex::encode_prefixed(&row.hash),
        canonical_hash: row.canonical_hash.as_ref().map(hex::encode_prefixed).unwrap_or_default(),
        timestamp: row.payload.block.header().timestamp,
        shadow_gas_used: shadow.gas_used,
        canonical_gas_used: canonical.as_ref().map(|c| c.gas_used),
        gas_diff_abs,
        gas_diff_pct,
        shadow_tx_count: shadow.transaction_count,
        canonical_tx_count: canonical.as_ref().map(|c| c.transaction_count),
        tx_count_diff,
        shadow_non_deposit_tx_count: shadow.non_deposit_tx_count,
        canonical_non_deposit_tx_count: canonical.as_ref().map(|c| c.non_deposit_tx_count),
        shadow_priority_fee_inversions: shadow.priority_fee_inversions,
        canonical_builder_version: canonical.as_ref().map(|c| c.builder_version.clone()),
        shadow_builder_version: shadow.builder_version,
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
        hash: hex::encode_prefixed(&row.hash),
        parent_hash: hex::encode_prefixed(header.parent_hash),
        timestamp: header.timestamp,
        gas_used: header.gas_used,
        gas_limit: header.gas_limit,
        base_fee_per_gas: header.base_fee_per_gas,
        reorged_out: row.reorged_out,
        canonical_hash: row.canonical_hash.as_ref().map(hex::encode_prefixed),
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

    use super::*;

    const SENDER: Address = Address::repeat_byte(0x22);
    const RECIPIENT: Address = Address::repeat_byte(0x11);

    fn sample_row() -> ShadowBlockRow {
        sample_row_with(false, None)
    }

    fn sample_row_with(reorged_out: bool, canonical_hash: Option<Vec<u8>>) -> ShadowBlockRow {
        sample_row_full(42, 21_000, "test", reorged_out, canonical_hash)
    }

    fn sample_row_full(
        number: i64,
        gas_used: u64,
        builder_version: &str,
        reorged_out: bool,
        canonical_hash: Option<Vec<u8>>,
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
            hash: vec![0xab; 32],
            reorged_out,
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
    fn block_detail_marks_canonical_row() {
        let detail = block_detail(&sample_row());
        assert!(!detail.reorged_out);
        assert!(detail.canonical_hash.is_none());
    }

    #[test]
    fn block_detail_exposes_shadow_status_and_replacement_hash() {
        let detail = block_detail(&sample_row_with(true, Some(vec![0xcd; 32])));
        assert!(detail.reorged_out);
        assert_eq!(
            detail.canonical_hash.as_deref(),
            Some(format!("0x{}", "cd".repeat(32)).as_str())
        );
    }

    #[test]
    fn parse_block_id_distinguishes_number_from_hash() {
        assert!(matches!(parse_block_id("42"), Ok(BlockId::Number(42))));
        assert!(matches!(parse_block_id("  42 "), Ok(BlockId::Number(42))));

        let hash = format!("0x{}", "ab".repeat(32));
        assert!(matches!(parse_block_id(&hash), Ok(BlockId::Hash(_))));

        assert!(matches!(parse_block_id("nothex"), Err(ApiError::BadRequest)));
        assert!(matches!(parse_block_id("0x1234"), Err(ApiError::BadRequest)));
    }

    #[test]
    fn shadow_block_summary_computes_signed_diffs_against_canonical() {
        let shadow = sample_row_full(100, 30_000, "shadow", true, Some(vec![0xcd; 32]));
        let canonical = sample_row_full(100, 20_000, "canonical", false, None);
        let summary = shadow_block_summary(&shadow, Some(&canonical));

        assert_eq!(summary.number, 100);
        assert_eq!(summary.canonical_hash, format!("0x{}", "cd".repeat(32)));
        assert_eq!(summary.shadow_gas_used, 30_000);
        assert_eq!(summary.canonical_gas_used, Some(20_000));
        assert_eq!(summary.gas_diff_abs, Some(10_000));
        assert_eq!(summary.gas_diff_pct, Some(50.0));
        assert_eq!(summary.shadow_builder_version, "shadow");
        assert_eq!(summary.canonical_builder_version.as_deref(), Some("canonical"));
        assert_eq!(summary.tx_count_diff, Some(0));
    }

    #[test]
    fn shadow_block_summary_reports_negative_diff_when_shadow_uses_less() {
        let shadow = sample_row_full(1, 10_000, "shadow", true, Some(vec![0xcd; 32]));
        let canonical = sample_row_full(1, 20_000, "canonical", false, None);
        let summary = shadow_block_summary(&shadow, Some(&canonical));

        assert_eq!(summary.gas_diff_abs, Some(-10_000));
        assert_eq!(summary.gas_diff_pct, Some(-50.0));
    }

    #[test]
    fn shadow_block_summary_omits_canonical_fields_when_absent() {
        let shadow = sample_row_full(7, 21_000, "shadow", true, Some(vec![0xcd; 32]));
        let summary = shadow_block_summary(&shadow, None);

        assert!(summary.canonical_gas_used.is_none());
        assert!(summary.gas_diff_abs.is_none());
        assert!(summary.gas_diff_pct.is_none());
        assert!(summary.canonical_tx_count.is_none());
        assert!(summary.tx_count_diff.is_none());
        assert!(summary.canonical_builder_version.is_none());
    }

    #[test]
    fn shadow_block_summary_guards_against_zero_canonical_gas() {
        let shadow = sample_row_full(9, 21_000, "shadow", true, Some(vec![0xcd; 32]));
        let canonical = sample_row_full(9, 0, "canonical", false, None);
        let summary = shadow_block_summary(&shadow, Some(&canonical));

        assert_eq!(summary.gas_diff_abs, Some(21_000));
        assert!(summary.gas_diff_pct.is_none());
    }
}
