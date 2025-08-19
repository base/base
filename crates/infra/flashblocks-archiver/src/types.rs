use alloy_primitives::{map::foldhash::HashMap, Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use chrono::{DateTime, Utc};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashblockMessage {
    pub payload_id: PayloadId,
    pub index: u64,
    pub base: Option<ExecutionPayloadBaseV1>,
    pub diff: ExecutionPayloadFlashblockDeltaV1,
    pub metadata: Metadata,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct Metadata {
    pub receipts: HashMap<B256, OpReceipt>,
    pub new_account_balances: HashMap<Address, U256>,
    pub block_number: u64,
}

// Database models
#[derive(Debug, sqlx::FromRow)]
pub struct Builder {
    pub id: Uuid,
    pub url: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Flashblock {
    pub id: Uuid,
    pub builder_id: Uuid,
    pub payload_id: String,
    pub flashblock_index: i64,
    pub block_number: i64,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Transaction {
    pub id: Uuid,
    pub flashblock_id: Uuid,
    pub builder_id: Uuid,
    pub payload_id: String,
    pub flashblock_index: i64,
    pub block_number: i64,
    pub tx_data: Vec<u8>,
    pub tx_index: i32,
    pub created_at: DateTime<Utc>,
}
