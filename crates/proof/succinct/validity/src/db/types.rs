use std::{fmt::Debug, sync::Arc};

use alloy_primitives::{Address, B256};
use anyhow::Result;
use base_proof_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use chrono::{Local, NaiveDateTime};
use serde_json::Value;
use sqlx::{FromRow, PgPool, types::BigDecimal};

/// Lifecycle status of a proof request.
#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestStatus {
    /// Not yet submitted for proving.
    #[default]
    Unrequested = 0,
    /// Witness is being generated.
    WitnessGeneration = 1,
    /// Program is being executed (mock mode).
    Execution = 2,
    /// Proof is being generated.
    Prove = 3,
    /// Proof generation is complete.
    Complete = 4,
    /// Proof has been relayed on-chain.
    Relayed = 5,
    /// Request failed.
    Failed = 6,
    /// Request was cancelled.
    Cancelled = 7,
}

impl From<i16> for RequestStatus {
    fn from(value: i16) -> Self {
        match value {
            0 => Self::Unrequested,
            1 => Self::WitnessGeneration,
            2 => Self::Execution,
            3 => Self::Prove,
            4 => Self::Complete,
            5 => Self::Relayed,
            6 => Self::Failed,
            7 => Self::Cancelled,
            _ => panic!("Invalid request status: {value}"),
        }
    }
}

/// Whether a proof request is for a range proof or an aggregation proof.
#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestType {
    /// Range (span) proof covering a contiguous block range.
    #[default]
    Range = 0,
    /// Aggregation proof combining multiple range proofs.
    Aggregation = 1,
}

impl From<i16> for RequestType {
    fn from(value: i16) -> Self {
        match value {
            0 => Self::Range,
            1 => Self::Aggregation,
            _ => panic!("Invalid request type: {value}"),
        }
    }
}

/// Whether a proof request uses real or mock proving.
#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestMode {
    /// Real proof generation via the SP1 prover network or cluster.
    #[default]
    Real = 0,
    /// Mock proof generation for testing.
    Mock = 1,
}

impl From<i16> for RequestMode {
    fn from(value: i16) -> Self {
        match value {
            0 => Self::Real,
            1 => Self::Mock,
            _ => panic!("Invalid request mode: {value}"),
        }
    }
}

/// A single proof request row persisted in the driver database.
#[derive(FromRow, Default, Clone, Debug)]
pub struct OPSuccinctRequest {
    /// Auto-incrementing primary key.
    pub id: i64,
    /// Current lifecycle status of this request.
    pub status: RequestStatus,
    /// Whether this is a range or aggregation proof request.
    pub req_type: RequestType,
    /// Whether this request uses real or mock proving.
    pub mode: RequestMode,
    /// First L2 block included in the proof range.
    pub start_block: i64,
    /// Last L2 block included in the proof range.
    pub end_block: i64,
    /// Timestamp when the request was created.
    pub created_at: NaiveDateTime,
    /// Timestamp of the most recent status update.
    pub updated_at: NaiveDateTime,
    /// Network-mode proof request identifier (B256).
    pub proof_request_id: Option<Vec<u8>>, //B256
    /// When the proof was submitted to the prover network.
    pub proof_request_time: Option<NaiveDateTime>,
    /// L1 block number checkpointed for the aggregation proof.
    pub checkpointed_l1_block_number: Option<i64>,
    /// L1 block hash checkpointed for the aggregation proof (B256).
    pub checkpointed_l1_block_hash: Option<Vec<u8>>, //B256
    /// JSON blob of per-block execution statistics.
    pub execution_statistics: Value,
    /// Duration of witness generation in seconds.
    pub witnessgen_duration: Option<i64>,
    /// Duration of execution in seconds.
    pub execution_duration: Option<i64>,
    /// Duration of proof generation in seconds.
    pub prove_duration: Option<i64>,
    /// Commitment hash of the range verification key (B256).
    pub range_vkey_commitment: Vec<u8>, //B256
    /// Hash of the aggregation verification key (B256).
    pub aggregation_vkey_hash: Option<Vec<u8>>, //B256
    /// Hash of the rollup configuration (B256).
    pub rollup_config_hash: Vec<u8>, //B256
    /// Transaction hash of the on-chain relay (B256).
    pub relay_tx_hash: Option<Vec<u8>>, //B256
    /// Serialized proof bytes.
    pub proof: Option<Vec<u8>>, // Bytes
    /// Total number of transactions across all blocks in the range.
    pub total_nb_transactions: i64,
    /// Total gas used across all blocks in the range.
    pub total_eth_gas_used: i64,
    /// Total L1 fees across all blocks in the range.
    pub total_l1_fees: BigDecimal,
    /// Total transaction fees across all blocks in the range.
    pub total_tx_fees: BigDecimal,
    /// L1 chain ID for this request.
    pub l1_chain_id: i64,
    /// L2 chain ID for this request.
    pub l2_chain_id: i64,
    /// Contract address associated with this request (Address).
    pub contract_address: Option<Vec<u8>>, //Address
    /// Prover address that submitted the proof (Address).
    pub prover_address: Option<Vec<u8>>, //Address
    /// L1 head block number used when creating this request.
    pub l1_head_block_number: Option<i64>, // L1 head block number used for request
    /// Cluster proof handle JSON for self-hosted cluster mode.
    /// Contains {"`proof_id"`: "...", "`proof_output_id"`: "..."} for handle reconstruction.
    /// NULL for network mode requests (which use `proof_request_id` BYTEA for B256 instead).
    pub cluster_proof_handle: Option<Value>,
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
        let block_data =
            fetcher.get_l2_block_data_range(start_block as u64, end_block as u64).await?;

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
}

/// PostgreSQL-backed database client for the validity driver.
#[derive(Debug)]
pub struct DriverDBClient {
    /// Connection pool to the driver database.
    pub pool: PgPool,
}
