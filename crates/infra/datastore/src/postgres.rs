use crate::traits::BundleDatastore;
use alloy_consensus::Transaction;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::hex::{FromHex, ToHexExt};
use alloy_primitives::{Address, B256, TxHash};
use alloy_provider::network::eip2718::Decodable2718;
use alloy_rpc_types_mev::EthSendBundle;
use anyhow::Result;
use op_alloy_consensus::OpTxEnvelope;
use sqlx::{
    PgPool,
    types::chrono::{DateTime, Utc},
};
use tracing::info;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "bundle_state", rename_all = "PascalCase")]
pub enum BundleState {
    Ready,
    IncludedByBuilder,
}

#[derive(sqlx::FromRow, Debug)]
struct BundleRow {
    id: Uuid,
    senders: Option<Vec<String>>,
    minimum_base_fee: Option<i64>,
    txn_hashes: Option<Vec<String>>,
    txs: Vec<String>,
    reverting_tx_hashes: Option<Vec<String>>,
    dropping_tx_hashes: Option<Vec<String>>,
    block_number: Option<i64>,
    min_timestamp: Option<i64>,
    max_timestamp: Option<i64>,
    #[sqlx(rename = "bundle_state")]
    state: BundleState,
    state_changed_at: DateTime<Utc>,
}

/// Filter criteria for selecting bundles
#[derive(Debug, Clone, Default)]
pub struct BundleFilter {
    pub base_fee: Option<i64>,
    pub block_number: Option<u64>,
    pub timestamp: Option<u64>,
    pub max_time_before: Option<u64>,
    pub status: Option<BundleState>,
    pub txn_hashes: Option<Vec<TxHash>>,
}

impl BundleFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_base_fee(mut self, base_fee: i64) -> Self {
        self.base_fee = Some(base_fee);
        self
    }

    pub fn valid_for_block(mut self, block_number: u64) -> Self {
        self.block_number = Some(block_number);
        self
    }

    pub fn valid_for_timestamp(mut self, timestamp: u64) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    pub fn with_status(mut self, status: BundleState) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_txn_hashes(mut self, txn_hashes: Vec<TxHash>) -> Self {
        self.txn_hashes = Some(txn_hashes);
        self
    }

    pub fn with_max_time_before(mut self, timestamp: u64) -> Self {
        self.max_time_before = Some(timestamp);
        self
    }
}

/// Extended bundle data that includes the original bundle plus extracted metadata
#[derive(Debug, Clone)]
pub struct BundleWithMetadata {
    pub bundle: EthSendBundle,
    pub txn_hashes: Vec<TxHash>,
    pub senders: Vec<Address>,
    pub min_base_fee: i64,
    pub state: BundleState,
    pub state_changed_at: DateTime<Utc>,
}

/// Statistics about bundles and transactions grouped by state
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleStats {
    pub ready_bundles: u64,
    pub ready_transactions: u64,
    pub included_by_builder_bundles: u64,
    pub included_by_builder_transactions: u64,
    pub total_bundles: u64,
    pub total_transactions: u64,
}

#[derive(Debug, Clone)]
pub struct BlockInfoRecord {
    pub block_number: u64,
    pub block_hash: B256,
    pub finalized: bool,
}

#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub latest_block_number: u64,
    pub latest_block_hash: B256,
    pub latest_finalized_block_number: Option<u64>,
    pub latest_finalized_block_hash: Option<B256>,
}

#[derive(Debug, Clone)]
pub struct BlockInfoUpdate {
    pub block_number: u64,
    pub block_hash: B256,
}

#[derive(sqlx::FromRow, Debug)]
struct BlockInfoRow {
    latest_block_number: Option<i64>,
    latest_block_hash: Option<String>,
    latest_finalized_block_number: Option<i64>,
    latest_finalized_block_hash: Option<String>,
}

/// PostgreSQL implementation of the BundleDatastore trait
#[derive(Debug, Clone)]
pub struct PostgresDatastore {
    pool: PgPool,
}

impl PostgresDatastore {
    pub async fn run_migrations(&self) -> Result<()> {
        info!(message = "running migrations");
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!(message = "migrations complete");
        Ok(())
    }

    pub async fn connect(url: String) -> Result<Self> {
        let pool = PgPool::connect(&url).await?;
        Ok(Self::new(pool))
    }

    /// Create a new PostgreSQL datastore instance
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn row_to_bundle_with_metadata(&self, row: BundleRow) -> Result<BundleWithMetadata> {
        let parsed_txs: Result<Vec<alloy_primitives::Bytes>, _> =
            row.txs.into_iter().map(|tx_hex| tx_hex.parse()).collect();

        let parsed_reverting_tx_hashes: Result<Vec<TxHash>, _> = row
            .reverting_tx_hashes
            .unwrap_or_default()
            .into_iter()
            .map(TxHash::from_hex)
            .collect();

        let parsed_dropping_tx_hashes: Result<Vec<TxHash>, _> = row
            .dropping_tx_hashes
            .unwrap_or_default()
            .into_iter()
            .map(TxHash::from_hex)
            .collect();

        let bundle = EthSendBundle {
            txs: parsed_txs?,
            block_number: row.block_number.unwrap_or(0) as u64,
            min_timestamp: row.min_timestamp.map(|t| t as u64),
            max_timestamp: row.max_timestamp.map(|t| t as u64),
            reverting_tx_hashes: parsed_reverting_tx_hashes?,
            replacement_uuid: Some(row.id.to_string()),
            dropping_tx_hashes: parsed_dropping_tx_hashes?,
            refund_percent: None,
            refund_recipient: None,
            refund_tx_hashes: Vec::new(),
            extra_fields: Default::default(),
        };

        let parsed_txn_hashes: Result<Vec<TxHash>, _> = row
            .txn_hashes
            .unwrap_or_default()
            .into_iter()
            .map(TxHash::from_hex)
            .collect();

        let parsed_senders: Result<Vec<Address>, _> = row
            .senders
            .unwrap_or_default()
            .into_iter()
            .map(Address::from_hex)
            .collect();

        Ok(BundleWithMetadata {
            bundle,
            txn_hashes: parsed_txn_hashes?,
            senders: parsed_senders?,
            min_base_fee: row.minimum_base_fee.unwrap_or(0),
            state: row.state,
            state_changed_at: row.state_changed_at,
        })
    }

    fn extract_bundle_metadata(
        &self,
        bundle: &EthSendBundle,
    ) -> Result<(Vec<String>, i64, Vec<String>)> {
        let mut senders = Vec::new();
        let mut txn_hashes = Vec::new();

        let mut min_base_fee = i64::MAX;

        for tx_bytes in &bundle.txs {
            let envelope = OpTxEnvelope::decode_2718_exact(tx_bytes)?;
            txn_hashes.push(envelope.hash().encode_hex_with_prefix());

            let sender = match envelope.recover_signer() {
                Ok(signer) => signer,
                Err(err) => return Err(err.into()),
            };

            senders.push(sender.encode_hex_with_prefix());
            min_base_fee = min_base_fee.min(envelope.max_fee_per_gas() as i64); // todo type and todo not right
        }

        let minimum_base_fee = if min_base_fee == i64::MAX {
            0
        } else {
            min_base_fee
        };

        Ok((senders, minimum_base_fee, txn_hashes))
    }
}

#[async_trait::async_trait]
impl BundleDatastore for PostgresDatastore {
    async fn insert_bundle(&self, bundle: EthSendBundle) -> Result<Uuid> {
        let id = Uuid::new_v4();

        let (senders, minimum_base_fee, txn_hashes) = self.extract_bundle_metadata(&bundle)?;

        let txs: Vec<String> = bundle
            .txs
            .iter()
            .map(|tx| tx.encode_hex_upper_with_prefix())
            .collect();
        let reverting_tx_hashes: Vec<String> = bundle
            .reverting_tx_hashes
            .iter()
            .map(|h| h.encode_hex_with_prefix())
            .collect();
        let dropping_tx_hashes: Vec<String> = bundle
            .dropping_tx_hashes
            .iter()
            .map(|h| h.encode_hex_with_prefix())
            .collect();

        sqlx::query!(
            r#"
            INSERT INTO bundles (
                id, bundle_state, senders, minimum_base_fee, txn_hashes, 
                txs, reverting_tx_hashes, dropping_tx_hashes, 
                block_number, min_timestamp, max_timestamp,
                created_at, updated_at, state_changed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW(), NOW(), NOW())
            "#,
            id,
            BundleState::Ready as BundleState,
            &senders,
            minimum_base_fee,
            &txn_hashes,
            &txs,
            &reverting_tx_hashes,
            &dropping_tx_hashes,
            bundle.block_number as i64,
            bundle.min_timestamp.map(|t| t as i64),
            bundle.max_timestamp.map(|t| t as i64),
        )
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    async fn get_bundle(&self, id: Uuid) -> Result<Option<BundleWithMetadata>> {
        let result = sqlx::query_as::<_, BundleRow>(
            r#"
            SELECT id, senders, minimum_base_fee, txn_hashes, txs, reverting_tx_hashes,
                   dropping_tx_hashes, block_number, min_timestamp, max_timestamp, bundle_state, state_changed_at
            FROM bundles
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        match result {
            Some(row) => {
                let bundle_with_metadata = self.row_to_bundle_with_metadata(row)?;
                Ok(Some(bundle_with_metadata))
            }
            None => Ok(None),
        }
    }

    async fn cancel_bundle(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM bundles WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn select_bundles(&self, filter: BundleFilter) -> Result<Vec<BundleWithMetadata>> {
        // Convert txn_hashes to string array for SQL binding
        let txn_hash_strings: Option<Vec<String>> = filter
            .txn_hashes
            .map(|hashes| hashes.iter().map(|h| h.encode_hex_with_prefix()).collect());

        let rows = sqlx::query_as::<_, BundleRow>(
            r#"
            SELECT id, senders, minimum_base_fee, txn_hashes, txs, reverting_tx_hashes,
                   dropping_tx_hashes, block_number, min_timestamp, max_timestamp, bundle_state, state_changed_at
            FROM bundles
            WHERE ($1::bigint IS NULL OR minimum_base_fee >= $1)
              AND ($2::bigint IS NULL OR block_number = $2 OR block_number IS NULL OR block_number = 0)
              AND ($3::bigint IS NULL OR min_timestamp <= $3 OR min_timestamp IS NULL)
              AND ($3::bigint IS NULL OR max_timestamp >= $3 OR max_timestamp IS NULL)
              AND ($4::bundle_state IS NULL OR bundle_state = $4)
              AND ($5::text[] IS NULL OR txn_hashes::text[] && $5)
              AND ($6::bigint IS NULL OR max_timestamp < $6)
            ORDER BY minimum_base_fee DESC
            "#,
        )
        .bind(filter.base_fee)
        .bind(filter.block_number.map(|n| n as i64))
        .bind(filter.timestamp.map(|t| t as i64))
        .bind(filter.status)
        .bind(txn_hash_strings)
        .bind(filter.max_time_before.map(|t| t as i64))
        .fetch_all(&self.pool)
        .await?;

        let mut bundles = Vec::new();
        for row in rows {
            let bundle_with_metadata = self.row_to_bundle_with_metadata(row)?;
            bundles.push(bundle_with_metadata);
        }

        Ok(bundles)
    }

    async fn find_bundle_by_transaction_hash(&self, tx_hash: TxHash) -> Result<Option<Uuid>> {
        let tx_hash_str = tx_hash.encode_hex_with_prefix();

        let result = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id 
            FROM bundles 
            WHERE $1 = ANY(txn_hashes)
            LIMIT 1
            "#,
        )
        .bind(&tx_hash_str)
        .fetch_optional(&self.pool)
        .await?;

        Ok(result)
    }

    async fn remove_bundles(&self, ids: Vec<Uuid>) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query("DELETE FROM bundles WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() as usize)
    }

    async fn update_bundles_state(
        &self,
        uuids: Vec<Uuid>,
        allowed_prev_states: Vec<BundleState>,
        new_state: BundleState,
    ) -> Result<Vec<Uuid>> {
        let prev_states_sql: Vec<String> = allowed_prev_states
            .iter()
            .map(|s| match s {
                BundleState::Ready => "Ready".to_string(),
                BundleState::IncludedByBuilder => "IncludedByBuilder".to_string(),
            })
            .collect();
        let rows = sqlx::query!(
            r#"
            UPDATE bundles 
            SET bundle_state = $1, updated_at = NOW(), state_changed_at = NOW()
            WHERE id = ANY($2) AND bundle_state::text = ANY($3)
            RETURNING id
            "#,
            new_state as BundleState,
            &uuids,
            &prev_states_sql
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|row| row.id).collect())
    }

    async fn get_current_block_info(&self) -> Result<Option<BlockInfo>> {
        let row = sqlx::query_as::<_, BlockInfoRow>(
            r#"
            SELECT
                (SELECT block_number FROM maintenance ORDER BY block_number DESC LIMIT 1) as latest_block_number,
                (SELECT block_hash FROM maintenance ORDER BY block_number DESC LIMIT 1) as latest_block_hash,
                (SELECT block_number FROM maintenance WHERE finalized = true ORDER BY block_number DESC LIMIT 1) as latest_finalized_block_number,
                (SELECT block_hash FROM maintenance WHERE finalized = true ORDER BY block_number DESC LIMIT 1) as latest_finalized_block_hash
            "#
        )
        .fetch_one(&self.pool)
        .await?;

        // If there's no latest block, return None
        let (latest_block_number, latest_block_hash) =
            match (row.latest_block_number, row.latest_block_hash) {
                (Some(block_number), Some(hash_str)) => {
                    let hash = B256::from_hex(&hash_str)
                        .map_err(|e| anyhow::anyhow!("Failed to parse latest block hash: {}", e))?;
                    (block_number as u64, hash)
                }
                _ => return Ok(None),
            };

        let latest_finalized_block_hash = if let Some(hash_str) = row.latest_finalized_block_hash {
            Some(B256::from_hex(&hash_str).map_err(|e| {
                anyhow::anyhow!("Failed to parse latest finalized block hash: {}", e)
            })?)
        } else {
            None
        };

        Ok(Some(BlockInfo {
            latest_block_number,
            latest_block_hash,
            latest_finalized_block_number: row.latest_finalized_block_number.map(|n| n as u64),
            latest_finalized_block_hash,
        }))
    }

    async fn commit_block_info(&self, blocks: Vec<BlockInfoUpdate>) -> Result<()> {
        for block in blocks {
            let block_hash_str = block.block_hash.encode_hex_with_prefix();

            sqlx::query!(
                r#"
                INSERT INTO maintenance (block_number, block_hash, finalized)
                VALUES ($1, $2, false)
                ON CONFLICT (block_number)
                DO UPDATE SET block_hash = EXCLUDED.block_hash, finalized = false
                "#,
                block.block_number as i64,
                block_hash_str,
            )
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn finalize_blocks_before(&self, block_number: u64) -> Result<u64> {
        let result = sqlx::query!(
            "UPDATE maintenance SET finalized = true WHERE block_number < $1 AND finalized = false",
            block_number as i64
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    async fn prune_finalized_blocks(&self, before_block_number: u64) -> Result<u64> {
        let result = sqlx::query!(
            "DELETE FROM maintenance WHERE finalized = true AND block_number < $1",
            before_block_number as i64
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    async fn get_stats(&self) -> Result<BundleStats> {
        let result = sqlx::query!(
            r#"
            SELECT
                bundle_state::text as bundle_state_text,
                COUNT(*) as bundle_count,
                SUM(COALESCE(array_length(txn_hashes, 1), 0)) as transaction_count
            FROM bundles
            GROUP BY bundle_state
            "#
        )
        .fetch_all(&self.pool)
        .await?;

        let mut stats = BundleStats {
            ready_bundles: 0,
            ready_transactions: 0,
            included_by_builder_bundles: 0,
            included_by_builder_transactions: 0,
            total_bundles: 0,
            total_transactions: 0,
        };

        for row in result {
            let bundle_count = row.bundle_count.unwrap_or(0) as u64;
            let transaction_count = row.transaction_count.unwrap_or(0) as u64;

            stats.total_bundles += bundle_count;
            stats.total_transactions += transaction_count;

            if let Some(state_text) = row.bundle_state_text {
                match state_text.as_str() {
                    "Ready" => {
                        stats.ready_bundles = bundle_count;
                        stats.ready_transactions = transaction_count;
                    }
                    "IncludedByBuilder" => {
                        stats.included_by_builder_bundles = bundle_count;
                        stats.included_by_builder_transactions = transaction_count;
                    }
                    _ => {
                        // Unknown state, just add to totals
                    }
                }
            }
        }

        Ok(stats)
    }

    async fn remove_timed_out_bundles(&self, current_time: u64) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            r#"
            DELETE FROM bundles
            WHERE bundle_state = 'Ready'
              AND max_timestamp IS NOT NULL
              AND max_timestamp < $1
            RETURNING id
            "#,
        )
        .bind(current_time as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn remove_old_included_bundles(
        &self,
        cutoff_timestamp: DateTime<Utc>,
    ) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            r#"
            DELETE FROM bundles
            WHERE bundle_state = 'IncludedByBuilder'
              AND state_changed_at < $1
            RETURNING id
            "#,
        )
        .bind(cutoff_timestamp)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}
