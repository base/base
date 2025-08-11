use crate::types::{Flashblock, FlashblockMessage};
use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use anyhow::Result;
use reth_optimism_primitives::OpReceipt;
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

    pub async fn new(database_url: &str, max_connections: u32) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;

        info!("Connected to database");
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!("Database migrations completed");
        Ok(())
    }

    pub async fn get_or_create_builder(&self, url: &str, name: Option<&str>) -> Result<Uuid> {
        let existing = sqlx::query_scalar::<_, Uuid>("SELECT id FROM builders WHERE url = $1")
            .bind(url)
            .fetch_optional(&self.pool)
            .await?;

        if let Some(id) = existing {
            return Ok(id);
        }

        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO builders (url, name) VALUES ($1, $2) RETURNING id",
        )
        .bind(url)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;

        info!(
            "Created new builder: {} ({})",
            name.unwrap_or("unnamed"),
            url
        );
        Ok(id)
    }

    pub async fn store_flashblock(
        &self,
        builder_id: Uuid,
        payload: &FlashblockMessage,
    ) -> Result<Uuid> {
        let raw_message = serde_json::to_value(payload)?;

        let flashblock_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO flashblocks (
                builder_id, payload_id, flashblock_index, block_number, raw_message,
                parent_beacon_block_root, parent_hash, fee_recipient, prev_randao,
                gas_limit, base_timestamp, extra_data, base_fee_per_gas,
                state_root, receipts_root, logs_bloom, gas_used, block_hash, withdrawals_root
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9,
                $10, $11, $12, $13,
                $14, $15, $16, $17, $18, $19
            ) RETURNING id
            "#,
        )
        .bind(builder_id)
        .bind(payload.payload_id.to_string())
        .bind(payload.index as i64)
        .bind(payload.metadata.block_number as i64)
        .bind(raw_message)
        // Base fields
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.parent_beacon_block_root)),
        )
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.parent_hash)),
        )
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.fee_recipient)),
        )
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.prev_randao)),
        )
        .bind(payload.base.as_ref().map(|b| b.gas_limit as i64))
        .bind(payload.base.as_ref().map(|b| b.timestamp as i64))
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.extra_data)),
        )
        .bind(
            payload
                .base
                .as_ref()
                .map(|b| format!("{:#x}", b.base_fee_per_gas)),
        )
        // Delta fields
        .bind(format!("{:#x}", payload.diff.state_root))
        .bind(format!("{:#x}", payload.diff.receipts_root))
        .bind(format!("{:#x}", payload.diff.logs_bloom))
        .bind(payload.diff.gas_used as i64)
        .bind(format!("{:#x}", payload.diff.block_hash))
        .bind(if payload.diff.withdrawals_root == B256::ZERO {
            None
        } else {
            Some(format!("{:#x}", payload.diff.withdrawals_root))
        })
        .fetch_one(&self.pool)
        .await?;

        // Store transactions
        for (tx_index, tx_data) in payload.diff.transactions.iter().enumerate() {
            self.store_transaction(
                flashblock_id,
                builder_id,
                &payload.payload_id.to_string(),
                payload.index as i64,
                payload.metadata.block_number as i64,
                tx_data.as_ref(),
                tx_index as i32,
            )
            .await?;
        }

        // Store withdrawals
        for withdrawal in &payload.diff.withdrawals {
            self.store_withdrawal(
                flashblock_id,
                builder_id,
                &payload.payload_id,
                payload.index as i64,
                payload.metadata.block_number as i64,
                withdrawal,
            )
            .await?;
        }

        // Store receipts from metadata
        for (tx_hash, receipt) in &payload.metadata.receipts {
            self.store_receipt(
                flashblock_id,
                builder_id,
                &payload.payload_id,
                payload.index as i64,
                payload.metadata.block_number as i64,
                tx_hash,
                receipt,
            )
            .await?;
        }

        // Store account balances from metadata
        for (address, balance) in &payload.metadata.new_account_balances {
            self.store_account_balance(
                flashblock_id,
                builder_id,
                &payload.payload_id,
                payload.index as i64,
                payload.metadata.block_number as i64,
                address,
                balance,
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
        tx_index: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO transactions (
                flashblock_id, builder_id, payload_id, flashblock_index, 
                block_number, tx_data, tx_index
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(flashblock_id)
        .bind(builder_id)
        .bind(payload_id)
        .bind(flashblock_index)
        .bind(block_number)
        .bind(tx_data)
        .bind(tx_index)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_withdrawal(
        &self,
        flashblock_id: Uuid,
        builder_id: Uuid,
        payload_id: &PayloadId,
        flashblock_index: i64,
        block_number: i64,
        withdrawal: &alloy_rpc_types::Withdrawal,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO withdrawals (
                flashblock_id, builder_id, payload_id, flashblock_index,
                block_number, withdrawal_index, validator_index, address, amount
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(flashblock_id)
        .bind(builder_id)
        .bind(payload_id.to_string())
        .bind(flashblock_index)
        .bind(block_number)
        .bind(withdrawal.index as i64)
        .bind(withdrawal.validator_index as i64)
        .bind(format!("{:#x}", withdrawal.address))
        .bind(withdrawal.amount as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_receipt(
        &self,
        flashblock_id: Uuid,
        builder_id: Uuid,
        payload_id: &PayloadId,
        flashblock_index: i64,
        block_number: i64,
        tx_hash: &B256,
        receipt: &OpReceipt,
    ) -> Result<()> {
        let (deposit_nonce, deposit_receipt_version) = match receipt {
            OpReceipt::Deposit(receipt) => (
                receipt.deposit_nonce.map(|n| n as i64),
                receipt.deposit_receipt_version.map(|v| v as i64),
            ),
            _ => (None, None),
        };

        sqlx::query(
            r#"
            INSERT INTO receipts (
                flashblock_id, builder_id, payload_id, flashblock_index, block_number,
                tx_hash, tx_type, status, cumulative_gas_used, logs, deposit_nonce, deposit_receipt_version
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(flashblock_id)
        .bind(builder_id)
        .bind(payload_id.to_string())
        .bind(flashblock_index)
        .bind(block_number)
        .bind(tx_hash.to_string())
        .bind(receipt.tx_type() as i32)
        .bind(receipt.status())
        .bind(receipt.cumulative_gas_used() as i64)
        .bind(serde_json::to_value(receipt.logs())?)
        .bind(deposit_nonce)
        .bind(deposit_receipt_version)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_account_balance(
        &self,
        flashblock_id: Uuid,
        builder_id: Uuid,
        payload_id: &PayloadId,
        flashblock_index: i64,
        block_number: i64,
        address: &Address,
        balance: &U256,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO account_balances (
                flashblock_id, builder_id, payload_id, flashblock_index,
                block_number, address, balance
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(flashblock_id)
        .bind(builder_id)
        .bind(payload_id.to_string())
        .bind(flashblock_index)
        .bind(block_number)
        .bind(address.to_string())
        .bind(balance.to_string())
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
