use alloy_primitives::{map::foldhash::HashMap, Address, B256, U256};
use alloy_rpc_types_engine::PayloadId;
use chrono::{DateTime, Utc};
use reth_optimism_primitives::OpReceipt;
use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    pub raw_message: Value,

    // Base payload fields
    pub parent_beacon_block_root: Option<String>,
    pub parent_hash: Option<String>,
    pub fee_recipient: Option<String>,
    pub prev_randao: Option<String>,
    pub gas_limit: Option<i64>,
    pub base_timestamp: Option<i64>,
    pub extra_data: Option<String>,
    pub base_fee_per_gas: Option<String>,

    // Delta fields
    pub state_root: String,
    pub receipts_root: String,
    pub logs_bloom: String,
    pub gas_used: i64,
    pub block_hash: String,
    pub withdrawals_root: Option<String>,
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

#[derive(Debug, sqlx::FromRow)]
pub struct WithdrawalRecord {
    pub id: Uuid,
    pub flashblock_id: Uuid,
    pub builder_id: Uuid,
    pub payload_id: String,
    pub flashblock_index: i64,
    pub block_number: i64,
    pub withdrawal_index: i64,
    pub validator_index: i64,
    pub address: String,
    pub amount: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct Receipt {
    pub id: Uuid,
    pub flashblock_id: Uuid,
    pub builder_id: Uuid,
    pub payload_id: String,
    pub flashblock_index: i64,
    pub block_number: i64,
    pub tx_hash: String,
    pub tx_type: i32, // 0=Legacy, 1=EIP-2930, 2=EIP-1559, 4=EIP-7702, 126=Deposit
    pub status: bool, // Transaction success/failure
    pub cumulative_gas_used: i64,
    pub logs: Value,                          // Array of log objects
    pub deposit_nonce: Option<i64>,           // Only for Optimism deposit transactions
    pub deposit_receipt_version: Option<i64>, // Only for Optimism deposit transactions
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct AccountBalance {
    pub id: Uuid,
    pub flashblock_id: Uuid,
    pub builder_id: Uuid,
    pub payload_id: String,
    pub flashblock_index: i64,
    pub block_number: i64,
    pub address: String,
    pub balance: String,
    pub created_at: DateTime<Utc>,
}
