use crate::traits::BundleDatastore;
use alloy_consensus::Transaction;
use alloy_consensus::private::alloy_eips::Decodable2718;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_primitives::hex::{FromHex, ToHexExt};
use alloy_primitives::{Address, TxHash};
use alloy_rpc_types_mev::EthSendBundle;
use anyhow::Result;
use op_alloy_consensus::OpTxEnvelope;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "bundle_state", rename_all = "PascalCase")]
pub enum BundleState {
    Ready,
    BundleLimit,
    AccountLimits,
    GlobalLimits,
    IncludedInFlashblock,
    IncludedInBlock,
}

#[derive(sqlx::FromRow, Debug)]
struct BundleRow {
    senders: Option<Vec<String>>,
    minimum_base_fee: Option<i64>,
    txn_hashes: Option<Vec<String>>,
    txs: Vec<String>,
    reverting_tx_hashes: Option<Vec<String>>,
    dropping_tx_hashes: Option<Vec<String>>,
    block_number: Option<i64>,
    min_timestamp: Option<i64>,
    max_timestamp: Option<i64>,
    state: BundleState,
}

/// Filter criteria for selecting bundles
#[derive(Debug, Clone, Default)]
pub struct BundleFilter {
    pub base_fee: Option<i64>,
    pub block_number: Option<u64>,
    pub timestamp: Option<u64>,
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
}

/// Extended bundle data that includes the original bundle plus extracted metadata
#[derive(Debug, Clone)]
pub struct BundleWithMetadata {
    pub bundle: EthSendBundle,
    pub txn_hashes: Vec<TxHash>,
    pub senders: Vec<Address>,
    pub min_base_fee: i64,
    pub state: BundleState,
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
            replacement_uuid: None,
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
                id, "state", senders, minimum_base_fee, txn_hashes, 
                txs, reverting_tx_hashes, dropping_tx_hashes, 
                block_number, min_timestamp, max_timestamp,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW(), NOW())
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
            SELECT senders, minimum_base_fee, txn_hashes, txs, reverting_tx_hashes, 
                   dropping_tx_hashes, block_number, min_timestamp, max_timestamp, "state"
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
        let base_fee = filter.base_fee.unwrap_or(0);
        let block_number = filter.block_number.unwrap_or(0) as i64;

        let (min_ts, max_ts) = if let Some(timestamp) = filter.timestamp {
            (timestamp as i64, timestamp as i64)
        } else {
            // If not specified, set the parameters to be the whole range
            (i64::MAX, 0i64)
        };

        let rows = sqlx::query_as::<_, BundleRow>(
            r#"
            SELECT senders, minimum_base_fee, txn_hashes, txs, reverting_tx_hashes, 
                   dropping_tx_hashes, block_number, min_timestamp, max_timestamp, "state"
            FROM bundles 
            WHERE minimum_base_fee >= $1
              AND (block_number = $2 OR block_number IS NULL OR block_number = 0 OR $2 = 0)
              AND (min_timestamp <= $3 OR min_timestamp IS NULL)
              AND (max_timestamp >= $4 OR max_timestamp IS NULL)
            ORDER BY minimum_base_fee DESC
            "#,
        )
        .bind(base_fee)
        .bind(block_number)
        .bind(min_ts)
        .bind(max_ts)
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

    async fn remove_bundle(&self, id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM bundles WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
