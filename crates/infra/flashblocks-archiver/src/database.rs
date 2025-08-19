use crate::types::{Flashblock, FlashblockMessage};
use alloy_primitives::keccak256;
use anyhow::Result;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

#[derive(Debug)]
pub struct Database {
    pool: PgPool,
}

impl Database {
    pub fn get_pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn new(database_url: &str, max_connections: u32) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;

        info!(message = "Connected to database");
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!(message = "Database migrations completed");
        Ok(())
    }

    pub async fn get_or_create_builder(&self, url: &str, name: Option<&str>) -> Result<Uuid> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO builders (url, name) 
            VALUES ($1, $2) 
            ON CONFLICT (url) 
            DO UPDATE SET 
                name = EXCLUDED.name,
                updated_at = NOW()
            RETURNING id
            "#,
        )
        .bind(url)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;

        Ok(id)
    }

    pub async fn store_flashblock(
        &self,
        builder_id: Uuid,
        payload: &FlashblockMessage,
    ) -> Result<Uuid> {
        let flashblock_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO flashblocks (
                builder_id, payload_id, flashblock_index, block_number
            ) VALUES (
                $1, $2, $3, $4
            ) 
            ON CONFLICT (builder_id, payload_id, flashblock_index) 
            DO UPDATE SET 
                block_number = EXCLUDED.block_number,
                received_at = NOW()
            RETURNING id
            "#,
        )
        .bind(builder_id)
        .bind(payload.payload_id.to_string())
        .bind(payload.index as i64)
        .bind(payload.metadata.block_number as i64)
        .fetch_one(&self.pool)
        .await?;

        for (tx_index, tx_data) in payload.diff.transactions.iter().enumerate() {
            let tx_hash = keccak256(tx_data);
            self.store_transaction(
                flashblock_id,
                builder_id,
                &payload.payload_id.to_string(),
                payload.index as i64,
                payload.metadata.block_number as i64,
                tx_data.as_ref(),
                &format!("{:#x}", tx_hash),
                tx_index as i32,
            )
            .await?;
        }

        Ok(flashblock_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_transaction(
        &self,
        flashblock_id: Uuid,
        builder_id: Uuid,
        payload_id: &str,
        flashblock_index: i64,
        block_number: i64,
        tx_data: &[u8],
        tx_hash: &str,
        tx_index: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO transactions (
                flashblock_id, builder_id, payload_id, flashblock_index, 
                block_number, tx_data, tx_hash, tx_index
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (flashblock_id, tx_index) 
            DO UPDATE SET 
                tx_data = EXCLUDED.tx_data,
                tx_hash = EXCLUDED.tx_hash,
                created_at = NOW()
            "#,
        )
        .bind(flashblock_id)
        .bind(builder_id)
        .bind(payload_id)
        .bind(flashblock_index)
        .bind(block_number)
        .bind(tx_data)
        .bind(tx_hash)
        .bind(tx_index)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_flashblocks_by_block_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<Flashblock>> {
        let flashblocks = sqlx::query_as::<_, Flashblock>(
            "SELECT * FROM flashblocks WHERE block_number = $1 ORDER BY flashblock_index",
        )
        .bind(block_number as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(flashblocks)
    }

    pub async fn get_latest_block_number(&self, builder_id: Uuid) -> Result<Option<u64>> {
        let result: (Option<i64>,) = sqlx::query_as(
            "SELECT MAX(block_number) as max_block FROM flashblocks WHERE builder_id = $1",
        )
        .bind(builder_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result.0.map(|b| b as u64))
    }
}
