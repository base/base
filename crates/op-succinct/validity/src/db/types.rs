use alloy_primitives::{Address, B256};
use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use serde_json::Value;
use sqlx::types::BigDecimal;
use sqlx::{FromRow, PgPool};
use std::fmt::Debug;
use std::sync::Arc;

#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestStatus {
    #[default]
    Unrequested = 0,
    WitnessGeneration = 1,
    Execution = 2,
    Prove = 3,
    Complete = 4,
    Relayed = 5,
    Failed = 6,
    Cancelled = 7,
}

impl From<i16> for RequestStatus {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestStatus::Unrequested,
            1 => RequestStatus::WitnessGeneration,
            2 => RequestStatus::Execution,
            3 => RequestStatus::Prove,
            4 => RequestStatus::Complete,
            5 => RequestStatus::Relayed,
            6 => RequestStatus::Failed,
            7 => RequestStatus::Cancelled,
            _ => panic!("Invalid request status: {}", value),
        }
    }
}

#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestType {
    #[default]
    Range = 0,
    Aggregation = 1,
}

impl From<i16> for RequestType {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestType::Range,
            1 => RequestType::Aggregation,
            _ => panic!("Invalid request type: {}", value),
        }
    }
}

#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestMode {
    #[default]
    Real = 0,
    Mock = 1,
}

impl From<i16> for RequestMode {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestMode::Real,
            1 => RequestMode::Mock,
            _ => panic!("Invalid request mode: {}", value),
        }
    }
}

#[derive(FromRow, Default, Clone)]
pub struct OPSuccinctRequest {
    pub id: i64,
    pub status: RequestStatus,
    pub req_type: RequestType,
    pub mode: RequestMode,
    pub start_block: i64,
    pub end_block: i64,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub proof_request_id: Option<Vec<u8>>, //B256
    pub proof_request_time: Option<NaiveDateTime>,
    pub checkpointed_l1_block_number: Option<i64>,
    pub checkpointed_l1_block_hash: Option<Vec<u8>>, //B256
    pub execution_statistics: Value,
    pub witnessgen_duration: Option<i64>,
    pub execution_duration: Option<i64>,
    pub prove_duration: Option<i64>,
    pub range_vkey_commitment: Vec<u8>,         //B256
    pub aggregation_vkey_hash: Option<Vec<u8>>, //B256
    pub rollup_config_hash: Vec<u8>,            //B256
    pub relay_tx_hash: Option<Vec<u8>>,         //B256
    pub proof: Option<Vec<u8>>,                 // Bytes
    pub total_nb_transactions: i64,
    pub total_eth_gas_used: i64,
    pub total_l1_fees: BigDecimal,
    pub total_tx_fees: BigDecimal,
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    pub contract_address: Option<Vec<u8>>, //Address
    pub prover_address: Option<Vec<u8>>,   //Address
    pub l1_head_block_number: Option<i64>, // L1 head block number used for request
}

impl OPSuccinctRequest {
    /// Creates a new range request and fetches the block data.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_range_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        rollup_config_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
        fetcher: Arc<OPSuccinctDataFetcher>,
    ) -> Result<Self> {
        let block_data = fetcher
            .get_l2_block_data_range(start_block as u64, end_block as u64)
            .await?;

        Ok(Self::new_range_request(
            mode,
            start_block,
            end_block,
            range_vkey_commitment,
            rollup_config_hash,
            block_data,
            l1_chain_id,
            l2_chain_id,
        ))
    }

    /// Create a new range request given the block data.
    #[allow(clippy::too_many_arguments)]
    pub fn new_range_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        rollup_config_hash: B256,
        block_data: Vec<BlockInfo>,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Self {
        let now = Local::now().naive_local();

        let total_nb_transactions: u64 = block_data.iter().map(|b| b.transaction_count).sum();
        let total_eth_gas_used: u64 = block_data.iter().map(|b| b.gas_used).sum();
        // Note: The transaction fees include the L1 fees.
        let total_l1_fees: u128 = block_data.iter().map(|b| b.total_l1_fees).sum();
        let total_tx_fees: u128 = block_data.iter().map(|b| b.total_tx_fees).sum();

        Self {
            id: 0,
            status: RequestStatus::Unrequested,
            req_type: RequestType::Range,
            mode,
            start_block,
            end_block,
            created_at: now,
            updated_at: now,
            range_vkey_commitment: range_vkey_commitment.to_vec(),
            rollup_config_hash: rollup_config_hash.to_vec(),
            total_nb_transactions: total_nb_transactions as i64,
            total_eth_gas_used: total_eth_gas_used as i64,
            total_l1_fees: total_l1_fees.into(),
            total_tx_fees: total_tx_fees.into(),
            l1_chain_id,
            l2_chain_id,
            ..Default::default()
        }
    }

    /// Create a new aggregation request.
    #[allow(clippy::too_many_arguments)]
    pub fn new_agg_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        aggregation_vkey_hash: B256,
        rollup_config_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
        checkpointed_l1_block_number: i64,
        checkpointed_l1_block_hash: B256,
        prover_address: Address,
    ) -> Self {
        let now = Local::now().naive_local();

        Self {
            id: 0,
            status: RequestStatus::Unrequested,
            req_type: RequestType::Aggregation,
            mode,
            start_block,
            end_block,
            created_at: now,
            updated_at: now,
            checkpointed_l1_block_number: Some(checkpointed_l1_block_number),
            checkpointed_l1_block_hash: Some(checkpointed_l1_block_hash.to_vec()),
            range_vkey_commitment: range_vkey_commitment.to_vec(),
            aggregation_vkey_hash: Some(aggregation_vkey_hash.to_vec()),
            rollup_config_hash: rollup_config_hash.to_vec(),
            l1_chain_id,
            l2_chain_id,
            prover_address: Some(prover_address.to_vec()),
            l1_head_block_number: None,
            ..Default::default()
        }
    }

    /// Creates a retry request.
    ///
    /// Preserves the request type, mode, block range, vkey commitments, rollup config hash, transaction metrics, and chain IDs.
    pub fn new_retry_request(existing_request: &OPSuccinctRequest) -> Self {
        // Retry the same request if splitting was not triggered.
        let mut new_request = existing_request.clone();
        new_request.id = 0;
        new_request.status = RequestStatus::Unrequested;
        new_request.created_at = Local::now().naive_local();
        new_request.updated_at = Local::now().naive_local();
        new_request.proof_request_id = None;
        new_request.proof_request_time = None;
        new_request.checkpointed_l1_block_number = None;
        new_request.checkpointed_l1_block_hash = None;
        new_request.execution_statistics = serde_json::Value::Null;
        new_request.witnessgen_duration = None;
        new_request.execution_duration = None;
        new_request.prove_duration = None;
        new_request.relay_tx_hash = None;
        new_request.proof = None;

        new_request
    }
}

pub struct DriverDBClient {
    pub pool: PgPool,
}
