use alloy_primitives::{Address, B256};
use anyhow::Result;
use serde_json::Value;
use sqlx::{
    postgres::{types::PgInterval, PgQueryResult},
    Error, PgPool,
};
use std::time::Duration;
use tracing::info;

use crate::{CommitmentConfig, DriverDBClient, OPSuccinctRequest, RequestStatus, RequestType};

impl DriverDBClient {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url).await?;

        // Run migrations.
        sqlx::migrate!("./migrations").run(&pool).await?;

        info!("Database configured successfully.");

        Ok(DriverDBClient { pool })
    }

    /// Adds a chain lock to the database.
    pub async fn add_chain_lock(
        &self,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            "INSERT INTO chain_locks (l1_chain_id, l2_chain_id, locked_at) 
             VALUES ($1, $2, NOW())
             ON CONFLICT (l1_chain_id, l2_chain_id) 
             DO UPDATE SET locked_at = NOW()",
            l1_chain_id,
            l2_chain_id
        )
        .execute(&self.pool)
        .await
    }

    /// Checks if a proposer already has a lock on the chain.
    pub async fn is_chain_locked(
        &self,
        l1_chain_id: i64,
        l2_chain_id: i64,
        interval: Duration,
    ) -> Result<bool, Error> {
        let result = sqlx::query!(
            r#"
            SELECT EXISTS(SELECT 1 FROM chain_locks WHERE l1_chain_id = $1 AND l2_chain_id = $2 AND locked_at > NOW() - $3::interval)
            "#,
            l1_chain_id,
            l2_chain_id,
            PgInterval::try_from(interval).unwrap()
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(result.exists.unwrap_or(false))
    }

    /// Updates the chain lock for a given chain.
    pub async fn update_chain_lock(
        &self,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            "UPDATE chain_locks SET locked_at = NOW() WHERE l1_chain_id = $1 AND l2_chain_id = $2",
            l1_chain_id,
            l2_chain_id
        )
        .execute(&self.pool)
        .await
    }

    /// Inserts a request into the database.
    pub async fn insert_request(&self, req: &OPSuccinctRequest) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            INSERT INTO requests (
                status,
                req_type,
                mode,
                start_block,
                end_block,
                created_at,
                updated_at,
                proof_request_id,
                proof_request_time,
                checkpointed_l1_block_number,
                checkpointed_l1_block_hash,
                execution_statistics,
                witnessgen_duration,
                execution_duration,
                prove_duration,
                range_vkey_commitment,
                aggregation_vkey_hash,
                rollup_config_hash,
                relay_tx_hash,
                proof,
                total_nb_transactions,
                total_eth_gas_used,
                total_l1_fees,
                total_tx_fees,
                l1_chain_id,
                l2_chain_id,
                contract_address,
                prover_address,
                l1_head_block_number
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29
            )
            "#,
            req.status as i16,
            req.req_type as i16,
            req.mode as i16,
            req.start_block as i64,
            req.end_block as i64,
            req.created_at,
            req.updated_at,
            req.proof_request_id.as_ref().map(|arr| &arr[..]),
            req.proof_request_time,
            req.checkpointed_l1_block_number.map(|n| n as i64),
            req.checkpointed_l1_block_hash.as_ref().map(|arr| &arr[..]),
            req.execution_statistics,
            // Storing durations in seconds.
            req.witnessgen_duration,
            req.execution_duration,
            req.prove_duration,
            &req.range_vkey_commitment,
            req.aggregation_vkey_hash.as_ref().map(|arr| &arr[..]),
            &req.rollup_config_hash,
            req.relay_tx_hash.as_ref().map(|arr| &arr[..]),
            req.proof.as_ref().map(|arr| &arr[..]),
            req.total_nb_transactions,
            req.total_eth_gas_used,
            req.total_l1_fees.clone(),
            req.total_tx_fees.clone(),
            req.l1_chain_id,
            req.l2_chain_id,
            req.contract_address.as_ref().map(|arr| &arr[..]),
            req.prover_address.as_ref().map(|arr| &arr[..]),
            req.l1_head_block_number.map(|n| n as i64),
        )
        .execute(&self.pool)
        .await
    }

    /// Fetch all requests with a specific block range and status FAILED.
    ///
    /// Checks that the request has the same range vkey commitment and rollup config hash as the
    /// commitment.
    pub async fn fetch_failed_request_count_by_block_range(
        &self,
        start_block: i64,
        end_block: i64,
        l1_chain_id: i64,
        l2_chain_id: i64,
        commitment: &CommitmentConfig,
    ) -> Result<i64, Error> {
        let count = sqlx::query!(
            "SELECT COUNT(*) FROM requests WHERE start_block = $1 AND end_block = $2 AND status = $3 AND range_vkey_commitment = $4 AND rollup_config_hash = $5 AND l1_chain_id = $6 AND l2_chain_id = $7",
            start_block as i64,
            end_block as i64,
            RequestStatus::Failed as i16,
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count.count.unwrap_or(0))
    }
    /// Fetch the highest end block of a request with one of the given statuses and commitment.
    pub async fn fetch_highest_end_block_for_range_request(
        &self,
        statuses: &[RequestStatus],
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<i64>, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        let result = sqlx::query!(
            "SELECT end_block FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = ANY($3) AND req_type = $4 AND l1_chain_id = $5 AND l2_chain_id = $6 ORDER BY end_block DESC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &status_values[..],
            RequestType::Range as i16,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(result.map(|r| r.end_block))
    }

    /// Fetch all range requests that have one of the given statuses and start block >=
    /// latest_contract_l2_block. Returns only the start and end block as tuples.
    pub async fn fetch_ranges_after_block(
        &self,
        statuses: &[RequestStatus],
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<(i64, i64)>, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        let ranges = sqlx::query!(
            "SELECT start_block, end_block FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = ANY($3) AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &status_values[..],
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(ranges.into_iter().map(|r| (r.start_block, r.end_block)).collect())
    }

    /// Fetch the number of requests with a specific status.
    pub async fn fetch_request_count(
        &self,
        status: RequestStatus,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<i64, Error> {
        let count = sqlx::query!(
            "SELECT COUNT(*) as count FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND l1_chain_id = $4 AND l2_chain_id = $5",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            status as i16,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count.count.unwrap_or(0))
    }

    /// Fetch all requests with a specific status from the database.
    pub async fn fetch_requests_by_status(
        &self,
        status: RequestStatus,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as::<_, OPSuccinctRequest>(
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND l1_chain_id = $4 AND l2_chain_id = $5"
        )
        .bind(&commitment.range_vkey_commitment[..])
        .bind(&commitment.rollup_config_hash[..])
        .bind(status as i16)
        .bind(l1_chain_id)
        .bind(l2_chain_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Get the consecutive range proofs for a given start block and end block that are complete
    /// with the same range vkey commitment.
    pub async fn get_consecutive_complete_range_proofs(
        &self,
        start_block: i64,
        end_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND end_block <= $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Range as i16,
            start_block,
            end_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch the checkpointed block hash and number for an aggregation request with the same start
    /// block, end block, and commitment config.
    pub async fn fetch_failed_agg_request_with_checkpointed_block_hash(
        &self,
        start_block: i64,
        end_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<(Vec<u8>, i64)>, Error> {
        let result = sqlx::query!(
            "SELECT checkpointed_l1_block_hash, checkpointed_l1_block_number FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND req_type = $4 AND start_block = $5 AND end_block = $6 AND status = $7 AND checkpointed_l1_block_hash IS NOT NULL AND checkpointed_l1_block_number IS NOT NULL AND l1_chain_id = $8 AND l2_chain_id = $9 LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestType::Aggregation as i16,
            start_block,
            end_block,
            RequestStatus::Failed as i16,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(result.map(|r| {
            (r.checkpointed_l1_block_hash.unwrap(), r.checkpointed_l1_block_number.unwrap())
        }))
    }
    /// Fetch the count of active (non-failed, non-cancelled) Aggregation proofs with the same start
    /// block, range vkey commitment, and aggregation vkey.
    pub async fn fetch_active_agg_proofs_count(
        &self,
        start_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<i64, Error> {
        let status_values: Vec<i16> = vec![
            RequestStatus::Unrequested as i16,
            RequestStatus::WitnessGeneration as i16,
            RequestStatus::Execution as i16,
            RequestStatus::Prove as i16,
            RequestStatus::Complete as i16,
        ];
        // Note: Relayed is not included in the status list in case the user re-starts the proposer
        // with the same DB and a different contract at the same starting block.
        let result = sqlx::query!(
            "SELECT COUNT(*) as count FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status = ANY($4) AND req_type = $5 AND start_block = $6 AND l1_chain_id = $7 AND l2_chain_id = $8",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            &status_values[..],
            RequestType::Aggregation as i16,
            start_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(result.count.unwrap_or(0))
    }

    /// Fetch the sorted list of Aggregation proofs with status Unrequested that have a start_block
    /// >= latest_contract_l2_block.
    ///
    /// Checks that the request has the same range vkey commitment, aggregation vkey, and rollup
    /// config hash as the commitment.
    ///
    /// NOTE: There should only be one "pending" Unrequested agg proof at a time for a specific
    /// start block.
    pub async fn fetch_unrequested_agg_proof(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status = $4 AND req_type = $5 AND start_block >= $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestStatus::Unrequested as i16,
            RequestType::Aggregation as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(request)
    }

    /// Fetch the first Range proof with status Unrequested that has a start_block >=
    /// latest_contract_l2_block.
    pub async fn fetch_first_unrequested_range_proof(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Unrequested as i16,
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    /// Fetch start and end blocks of all completed range proofs with matching range_vkey_commitment
    /// and start_block >= latest_contract_l2_block
    pub async fn fetch_completed_ranges(
        &self,
        commitment: &CommitmentConfig,
        latest_contract_l2_block: i64,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<(i64, i64)>, Error> {
        let blocks = sqlx::query!(
            "SELECT start_block, end_block FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7 ORDER BY start_block ASC",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(blocks.iter().map(|block| (block.start_block, block.end_block)).collect())
    }

    /// Update the l1_head_block_number for a request.
    pub async fn update_l1_head_block_number(
        &self,
        id: i64,
        l1_head_block_number: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET l1_head_block_number = $1 WHERE id = $2
            "#,
            l1_head_block_number,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the prove_duration based on the current time and the proof_request_time.
    pub async fn update_prove_duration(&self, id: i64) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET prove_duration = EXTRACT(EPOCH FROM (NOW() - proof_request_time))::BIGINT WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Add a completed proof to the database.
    pub async fn update_proof_to_complete(
        &self,
        id: i64,
        proof: &[u8],
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET proof = $1, status = $2, updated_at = NOW() WHERE id = $3
            "#,
            proof,
            RequestStatus::Complete as i16,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the witness generation duration of a request in the database.
    pub async fn update_witnessgen_duration(
        &self,
        id: i64,
        duration: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET witnessgen_duration = $1, updated_at = NOW() WHERE id = $2
            "#,
            duration,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the status of a request in the database.
    pub async fn update_request_status(
        &self,
        id: i64,
        new_status: RequestStatus,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests 
            SET status = $1, updated_at = NOW()
            WHERE id = $2
            "#,
            new_status as i16,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the status of a request to Prove.
    ///
    /// Updates the proof_request_time to the current time.
    pub async fn update_request_to_prove(
        &self,
        id: i64,
        proof_id: B256,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests 
            SET status = $1, proof_request_id = $2, proof_request_time = NOW(), updated_at = NOW()
            WHERE id = $3
            "#,
            RequestStatus::Prove as i16,
            proof_id.to_vec(),
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update status of a request to RELAYED.
    pub async fn update_request_to_relayed(
        &self,
        id: i64,
        relay_tx_hash: B256,
        contract_address: Address,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1, relay_tx_hash = $2, contract_address = $3, updated_at = NOW() WHERE id = $4
            "#,
            RequestStatus::Relayed as i16,
            relay_tx_hash.to_vec(),
            contract_address.to_vec(),
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Fetch a single completed aggregation proof after the given start block.
    pub async fn fetch_completed_agg_proof_after_block(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status = $4 AND req_type = $5 AND start_block = $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Aggregation as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    /// Insert the execution statistics for a request.
    pub async fn insert_execution_statistics(
        &self,
        id: i64,
        execution_statistics: Value,
        execution_duration_secs: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET execution_statistics = $1, execution_duration = $2 WHERE id = $3
            "#,
            &execution_statistics,
            execution_duration_secs,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Cancel all requests with the given status
    pub async fn cancel_all_requests_with_statuses(
        &self,
        statuses: &[RequestStatus],
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1 WHERE status = ANY($2) AND l1_chain_id = $3 AND l2_chain_id = $4
            "#,
            RequestStatus::Cancelled as i16,
            &status_values[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Drop all requests with the given statuses
    pub async fn delete_all_requests_with_statuses(
        &self,
        statuses: &[RequestStatus],
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        sqlx::query!(
            r#"
            DELETE FROM requests WHERE status = ANY($1) AND l1_chain_id = $2 AND l2_chain_id = $3
            "#,
            &status_values[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Cancel all prove requests with a different commitment config and same chain id's
    pub async fn cancel_prove_requests_with_different_commitment_config(
        &self,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1 WHERE status = $2 AND (range_vkey_commitment != $3 OR rollup_config_hash != $4) AND l1_chain_id = $5 AND l2_chain_id = $6
            "#,
            RequestStatus::Cancelled as i16,
            RequestStatus::Prove as i16,
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Batch insert requests.
    pub async fn insert_requests(
        &self,
        requests: &[OPSuccinctRequest],
    ) -> Result<PgQueryResult, Error> {
        // Process in batches to avoid PostgreSQL parameter limit of 65535.
        const BATCH_SIZE: usize = 100;

        // Use a transaction for better performance and atomicity
        let mut tx = self.pool.begin().await?;

        for chunk in requests.chunks(BATCH_SIZE) {
            let mut query_builder = sqlx::QueryBuilder::new(
                "INSERT INTO requests (
                    status, req_type, mode, start_block, end_block, created_at, updated_at,
                    proof_request_id, proof_request_time, checkpointed_l1_block_number,
                    checkpointed_l1_block_hash, execution_statistics, witnessgen_duration,
                    execution_duration, prove_duration, range_vkey_commitment,
                    aggregation_vkey_hash, rollup_config_hash, relay_tx_hash, proof,
                    total_nb_transactions, total_eth_gas_used, total_l1_fees, total_tx_fees,
                    l1_chain_id, l2_chain_id, contract_address, prover_address, l1_head_block_number) ",
            );

            query_builder.push_values(chunk, |mut b, req| {
                b.push_bind(req.status as i16)
                    .push_bind(req.req_type as i16)
                    .push_bind(req.mode as i16)
                    .push_bind(req.start_block)
                    .push_bind(req.end_block)
                    .push_bind(req.created_at)
                    .push_bind(req.updated_at)
                    .push_bind(req.proof_request_id.as_ref().map(|arr| &arr[..]))
                    .push_bind(req.proof_request_time)
                    .push_bind(req.checkpointed_l1_block_number)
                    .push_bind(req.checkpointed_l1_block_hash.as_ref().map(|arr| &arr[..]))
                    .push_bind(&req.execution_statistics)
                    .push_bind(req.witnessgen_duration)
                    .push_bind(req.execution_duration)
                    .push_bind(req.prove_duration)
                    .push_bind(&req.range_vkey_commitment[..])
                    .push_bind(req.aggregation_vkey_hash.as_ref().map(|arr| &arr[..]))
                    .push_bind(&req.rollup_config_hash[..])
                    .push_bind(req.relay_tx_hash.as_ref())
                    .push_bind(req.proof.as_ref())
                    .push_bind(req.total_nb_transactions)
                    .push_bind(req.total_eth_gas_used)
                    .push_bind(req.total_l1_fees.clone())
                    .push_bind(req.total_tx_fees.clone())
                    .push_bind(req.l1_chain_id)
                    .push_bind(req.l2_chain_id)
                    .push_bind(req.contract_address.as_ref().map(|arr| &arr[..]))
                    .push_bind(req.prover_address.as_ref().map(|arr| &arr[..]))
                    .push_bind(req.l1_head_block_number);
            });

            query_builder.build().execute(&mut *tx).await?;
        }

        // Commit the transaction
        tx.commit().await?;

        // Create a result with the total rows affected
        Ok(PgQueryResult::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitmentConfig, OPSuccinctRequest, RequestMode, RequestStatus, RequestType};
    use alloy_primitives::B256;
    use chrono::Local;
    use postgresql_embedded::{PostgreSQL, Settings};
    use sqlx::types::BigDecimal;
    use std::str::FromStr;

    // ==================== Test Harness ====================

    /// A test database instance that manages an embedded PostgreSQL server.
    /// Automatically cleans up when dropped.
    struct TestDb {
        #[allow(dead_code)] // Held to keep embedded PostgreSQL alive
        postgresql: PostgreSQL,
        client: DriverDBClient,
    }

    impl TestDb {
        /// Creates a new test database with migrations applied.
        async fn new() -> Self {
            let settings = Settings::default();
            let mut postgresql = PostgreSQL::new(settings);
            postgresql.setup().await.expect("Failed to setup PostgreSQL");
            postgresql.start().await.expect("Failed to start PostgreSQL");

            let database_url = postgresql.settings().url("postgres");
            let client = DriverDBClient::new(&database_url)
                .await
                .expect("Failed to connect to test database");

            Self { postgresql, client }
        }

        /// Returns a reference to the database client.
        fn client(&self) -> &DriverDBClient {
            &self.client
        }
    }

    // ==================== Test Fixtures ====================

    /// Default chain IDs used in tests.
    const L1ID: i64 = 1;
    const L2ID: i64 = 10;

    /// Creates a default commitment config for testing.
    fn default_commitment() -> CommitmentConfig {
        CommitmentConfig {
            range_vkey_commitment: B256::ZERO,
            agg_vkey_hash: B256::ZERO,
            rollup_config_hash: B256::ZERO,
        }
    }

    /// Builder for creating test requests with sensible defaults.
    struct RequestBuilder {
        start_block: i64,
        end_block: i64,
        status: RequestStatus,
        req_type: RequestType,
        mode: RequestMode,
        range_vkey_commitment: B256,
        aggregation_vkey_hash: Option<B256>,
        rollup_config_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
    }

    impl Default for RequestBuilder {
        fn default() -> Self {
            Self {
                start_block: 0,
                end_block: 10,
                status: RequestStatus::Unrequested,
                req_type: RequestType::Range,
                mode: RequestMode::Real,
                range_vkey_commitment: B256::ZERO,
                aggregation_vkey_hash: None,
                rollup_config_hash: B256::ZERO,
                l1_chain_id: L1ID,
                l2_chain_id: L2ID,
            }
        }
    }

    impl RequestBuilder {
        fn new() -> Self {
            Self::default()
        }

        fn range(mut self, start: i64, end: i64) -> Self {
            self.start_block = start;
            self.end_block = end;
            self
        }

        fn status(mut self, status: RequestStatus) -> Self {
            self.status = status;
            self
        }

        fn req_type(mut self, req_type: RequestType) -> Self {
            self.req_type = req_type;
            self
        }

        fn commitment(mut self, range_vkey: B256, rollup_config: B256) -> Self {
            self.range_vkey_commitment = range_vkey;
            self.rollup_config_hash = rollup_config;
            self
        }

        fn chains(mut self, l1: i64, l2: i64) -> Self {
            self.l1_chain_id = l1;
            self.l2_chain_id = l2;
            self
        }

        fn agg_vkey(mut self, agg_vkey: B256) -> Self {
            self.aggregation_vkey_hash = Some(agg_vkey);
            self
        }

        fn build(self) -> OPSuccinctRequest {
            let now = Local::now().naive_local();
            OPSuccinctRequest {
                id: 0,
                status: self.status,
                req_type: self.req_type,
                mode: self.mode,
                start_block: self.start_block,
                end_block: self.end_block,
                created_at: now,
                updated_at: now,
                proof_request_id: None,
                proof_request_time: None,
                checkpointed_l1_block_number: None,
                checkpointed_l1_block_hash: None,
                execution_statistics: serde_json::json!({}),
                witnessgen_duration: None,
                execution_duration: None,
                prove_duration: None,
                range_vkey_commitment: self.range_vkey_commitment.to_vec(),
                aggregation_vkey_hash: self.aggregation_vkey_hash.map(|h| h.to_vec()),
                rollup_config_hash: self.rollup_config_hash.to_vec(),
                relay_tx_hash: None,
                proof: None,
                total_nb_transactions: 0,
                total_eth_gas_used: 0,
                total_l1_fees: BigDecimal::from_str("0").unwrap(),
                total_tx_fees: BigDecimal::from_str("0").unwrap(),
                l1_chain_id: self.l1_chain_id,
                l2_chain_id: self.l2_chain_id,
                contract_address: None,
                prover_address: None,
                l1_head_block_number: None,
            }
        }
    }

    // ==================== Helper Functions ====================

    /// Inserts multiple requests into the database.
    async fn insert_requests(client: &DriverDBClient, requests: &[OPSuccinctRequest]) {
        for req in requests {
            client.insert_request(req).await.expect("Failed to insert request");
        }
    }

    /// Shorthand for fetching request count with default commitment and chain IDs.
    async fn count(c: &DriverDBClient, status: RequestStatus) -> i64 {
        c.fetch_request_count(status, &default_commitment(), L1ID, L2ID).await.unwrap()
    }

    /// A completed Range request - the most common fixture.
    fn completed_range(start: i64, end: i64) -> OPSuccinctRequest {
        RequestBuilder::new().range(start, end).status(RequestStatus::Complete).build()
    }

    /// An Aggregation request with given status.
    fn agg_request(start: i64, end: i64, status: RequestStatus) -> OPSuccinctRequest {
        RequestBuilder::new()
            .range(start, end)
            .status(status)
            .req_type(RequestType::Aggregation)
            .agg_vkey(B256::ZERO)
            .build()
    }

    // ==================== Tests ====================

    #[tokio::test]
    async fn test_chain_lock_prevents_concurrent_proposers() {
        let db = TestDb::new().await;
        let interval = Duration::from_secs(60);
        let c = db.client();

        assert!(!c.is_chain_locked(L1ID, L2ID, interval).await.unwrap());

        c.add_chain_lock(L1ID, L2ID).await.unwrap();

        assert!(c.is_chain_locked(L1ID, L2ID, interval).await.unwrap());
        assert!(!c.is_chain_locked(L1ID, 999, interval).await.unwrap());
        assert!(!c.is_chain_locked(999, L2ID, interval).await.unwrap());
    }

    /// Tests batch chunking logic (BATCH_SIZE = 100) to avoid PostgreSQL parameter limit.
    #[tokio::test]
    async fn test_insert_requests_handles_large_batches() {
        let db = TestDb::new().await;
        let c = db.client();

        let requests: Vec<_> =
            (0..150).map(|i| RequestBuilder::new().range(i * 10, (i + 1) * 10).build()).collect();

        c.insert_requests(&requests).await.unwrap();

        assert_eq!(count(c, RequestStatus::Unrequested).await, 150);
    }

    mod fetch_completed_ranges {
        use super::*;

        async fn fetch(db: &TestDb, start_block: i64) -> Vec<(i64, i64)> {
            db.client()
                .fetch_completed_ranges(&default_commitment(), start_block, L1ID, L2ID)
                .await
                .expect("fetch_completed_ranges failed")
        }

        #[tokio::test]
        async fn test_returns_ordered_by_start_block() {
            let db = TestDb::new().await;

            let requests = vec![
                completed_range(100, 110),
                completed_range(300, 310),
                completed_range(200, 210),
            ];
            insert_requests(db.client(), &requests).await;

            assert_eq!(fetch(&db, 0).await, vec![(100, 110), (200, 210), (300, 310)]);
        }

        #[tokio::test]
        async fn test_filters_by_status() {
            let db = TestDb::new().await;

            let requests = vec![
                completed_range(100, 110),
                RequestBuilder::new().range(200, 210).status(RequestStatus::Failed).build(),
                RequestBuilder::new().range(300, 310).status(RequestStatus::Unrequested).build(),
            ];
            insert_requests(db.client(), &requests).await;

            assert_eq!(fetch(&db, 0).await, vec![(100, 110)]);
        }

        #[tokio::test]
        async fn test_filters_by_start_block_threshold() {
            let db = TestDb::new().await;

            let requests =
                vec![completed_range(50, 60), completed_range(100, 110), completed_range(200, 210)];
            insert_requests(db.client(), &requests).await;

            assert_eq!(fetch(&db, 100).await, vec![(100, 110), (200, 210)]);
        }

        #[tokio::test]
        async fn test_filters_by_request_type() {
            let db = TestDb::new().await;

            let requests = vec![
                completed_range(100, 110),
                RequestBuilder::new()
                    .range(200, 210)
                    .status(RequestStatus::Complete)
                    .req_type(RequestType::Aggregation)
                    .build(),
            ];
            insert_requests(db.client(), &requests).await;

            assert_eq!(fetch(&db, 0).await, vec![(100, 110)]);
        }

        #[tokio::test]
        async fn test_filters_by_commitment_config() {
            let db = TestDb::new().await;
            let c = db.client();

            fn commit(range_vkey: u8, rollup_config: u8) -> CommitmentConfig {
                CommitmentConfig {
                    range_vkey_commitment: B256::repeat_byte(range_vkey),
                    agg_vkey_hash: B256::ZERO,
                    rollup_config_hash: B256::repeat_byte(rollup_config),
                }
            }

            let requests = vec![
                completed_range(100, 200),
                RequestBuilder::new()
                    .range(200, 300)
                    .status(RequestStatus::Complete)
                    .commitment(B256::repeat_byte(0x01), B256::ZERO)
                    .build(),
                RequestBuilder::new()
                    .range(300, 400)
                    .status(RequestStatus::Complete)
                    .commitment(B256::ZERO, B256::repeat_byte(0x02))
                    .build(),
            ];
            insert_requests(c, &requests).await;

            for (comm, expected) in [
                (commit(0x00, 0x00), vec![(100, 200)]),
                (commit(0x01, 0x00), vec![(200, 300)]),
                (commit(0x00, 0x02), vec![(300, 400)]),
            ] {
                let result = c.fetch_completed_ranges(&comm, 0, L1ID, L2ID).await.unwrap();
                assert_eq!(result, expected);
            }
        }
    }

    #[tokio::test]
    async fn test_get_consecutive_complete_range_proofs_returns_ordered() {
        let db = TestDb::new().await;
        let c = db.client();

        let requests =
            vec![completed_range(200, 300), completed_range(100, 200), completed_range(300, 400)];
        insert_requests(c, &requests).await;

        let result = c
            .get_consecutive_complete_range_proofs(100, 400, &default_commitment(), L1ID, L2ID)
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].start_block, 100);
        assert_eq!(result[1].start_block, 200);
        assert_eq!(result[2].start_block, 300);
    }

    #[tokio::test]
    async fn test_fetch_active_agg_proofs_count_excludes_inactive_statuses() {
        let db = TestDb::new().await;
        let c = db.client();

        let requests = vec![
            // Active statuses (should be counted)
            agg_request(100, 200, RequestStatus::Unrequested),
            agg_request(100, 200, RequestStatus::WitnessGeneration),
            agg_request(100, 200, RequestStatus::Execution),
            agg_request(100, 200, RequestStatus::Prove),
            agg_request(100, 200, RequestStatus::Complete),
            // Inactive statuses (should NOT be counted)
            agg_request(100, 200, RequestStatus::Failed),
            agg_request(100, 200, RequestStatus::Cancelled),
            agg_request(100, 200, RequestStatus::Relayed),
        ];
        insert_requests(c, &requests).await;

        let count =
            c.fetch_active_agg_proofs_count(100, &default_commitment(), L1ID, L2ID).await.unwrap();

        assert_eq!(count, 5);
    }

    #[tokio::test]
    async fn test_fetch_first_unrequested_range_proof_returns_lowest_start_block() {
        let db = TestDb::new().await;
        let c = db.client();

        let requests = vec![
            RequestBuilder::new().range(300, 400).build(),
            RequestBuilder::new().range(100, 200).build(),
            RequestBuilder::new().range(200, 300).build(),
            RequestBuilder::new().range(50, 100).status(RequestStatus::Complete).build(),
        ];
        insert_requests(c, &requests).await;

        let result = c
            .fetch_first_unrequested_range_proof(0, &default_commitment(), L1ID, L2ID)
            .await
            .unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap().start_block, 100); // Skips 50-100 (Complete status)
    }

    #[tokio::test]
    async fn test_queries_isolate_by_chain_id() {
        let db = TestDb::new().await;
        let c = db.client();

        // Insert: one for our chain, one for different L2, one for different L1
        let range_requests = vec![
            completed_range(100, 200),
            RequestBuilder::new()
                .range(100, 200)
                .status(RequestStatus::Complete)
                .chains(1, 999)
                .build(),
            RequestBuilder::new()
                .range(100, 200)
                .status(RequestStatus::Complete)
                .chains(999, 10)
                .build(),
        ];
        insert_requests(c, &range_requests).await;

        // Only our chain pair is returned
        let ranges = c.fetch_completed_ranges(&default_commitment(), 0, L1ID, L2ID).await.unwrap();
        assert_eq!(ranges, vec![(100, 200)]);

        // Aggregation count also isolated
        let agg_requests = vec![
            agg_request(100, 200, RequestStatus::Unrequested),
            RequestBuilder::new()
                .range(100, 200)
                .status(RequestStatus::Unrequested)
                .req_type(RequestType::Aggregation)
                .agg_vkey(B256::ZERO)
                .chains(1, 999)
                .build(),
        ];
        insert_requests(c, &agg_requests).await;

        let agg_count =
            c.fetch_active_agg_proofs_count(100, &default_commitment(), L1ID, L2ID).await.unwrap();
        assert_eq!(agg_count, 1);
    }

    // ==================== Status Transition Tests ====================

    mod status_transitions {
        use super::*;

        /// Helper: insert a request and return its database ID.
        async fn insert_and_get_id(c: &DriverDBClient, req: &OPSuccinctRequest) -> i64 {
            c.insert_request(req).await.unwrap();
            let requests = c
                .fetch_requests_by_status(req.status, &default_commitment(), L1ID, L2ID)
                .await
                .unwrap();
            requests.into_iter().find(|r| r.start_block == req.start_block).unwrap().id
        }

        #[tokio::test]
        async fn test_update_request_status_changes_status() {
            let db = TestDb::new().await;
            let c = db.client();

            let req = RequestBuilder::new().range(100, 200).build();
            let id = insert_and_get_id(c, &req).await;

            c.update_request_status(id, RequestStatus::WitnessGeneration).await.unwrap();

            assert_eq!(count(c, RequestStatus::WitnessGeneration).await, 1);
            assert_eq!(count(c, RequestStatus::Unrequested).await, 0);
        }

        #[tokio::test]
        async fn test_update_request_status_updates_timestamp() {
            let db = TestDb::new().await;
            let c = db.client();

            let req = RequestBuilder::new().range(100, 200).build();
            let original_updated_at = req.updated_at;
            let id = insert_and_get_id(c, &req).await;

            // Small delay to ensure timestamp difference
            tokio::time::sleep(Duration::from_millis(10)).await;

            c.update_request_status(id, RequestStatus::Prove).await.unwrap();

            let requests = c
                .fetch_requests_by_status(RequestStatus::Prove, &default_commitment(), L1ID, L2ID)
                .await
                .unwrap();
            let updated = requests.first().unwrap();
            assert!(updated.updated_at > original_updated_at);
        }
    }

    // ==================== Request Count Tests ====================

    #[tokio::test]
    async fn test_fetch_request_count_accuracy_across_statuses() {
        let db = TestDb::new().await;
        let c = db.client();

        let requests = vec![
            RequestBuilder::new().range(100, 200).status(RequestStatus::Unrequested).build(),
            RequestBuilder::new().range(200, 300).status(RequestStatus::Unrequested).build(),
            RequestBuilder::new().range(300, 400).status(RequestStatus::Prove).build(),
            RequestBuilder::new().range(400, 500).status(RequestStatus::Complete).build(),
            RequestBuilder::new().range(500, 600).status(RequestStatus::Failed).build(),
        ];
        insert_requests(c, &requests).await;

        assert_eq!(count(c, RequestStatus::Unrequested).await, 2);
        assert_eq!(count(c, RequestStatus::Prove).await, 1);
        assert_eq!(count(c, RequestStatus::Complete).await, 1);
        assert_eq!(count(c, RequestStatus::Failed).await, 1);
        assert_eq!(count(c, RequestStatus::Cancelled).await, 0);
    }

    // ==================== Aggregation Fetch Tests ====================

    mod aggregation_fetches {
        use super::*;

        #[tokio::test]
        async fn test_fetch_unrequested_agg_proof_ordering_and_threshold() {
            let db = TestDb::new().await;
            let c = db.client();

            let requests = vec![
                agg_request(50, 100, RequestStatus::Unrequested), // Below threshold
                agg_request(300, 400, RequestStatus::Unrequested),
                agg_request(100, 200, RequestStatus::Unrequested),
                agg_request(200, 300, RequestStatus::Unrequested),
            ];
            insert_requests(c, &requests).await;

            // Query from 100: should skip 50-100, return 100-200 (lowest above threshold)
            let result = c
                .fetch_unrequested_agg_proof(100, &default_commitment(), L1ID, L2ID)
                .await
                .unwrap();
            assert!(result.is_some());
            assert_eq!(result.unwrap().start_block, 100);
        }

        #[tokio::test]
        async fn test_fetch_completed_agg_proof_after_block() {
            let db = TestDb::new().await;
            let c = db.client();

            let requests = vec![
                agg_request(100, 200, RequestStatus::Complete),
                agg_request(100, 200, RequestStatus::Unrequested), // Same range, different status
            ];
            insert_requests(c, &requests).await;

            let result = c
                .fetch_completed_agg_proof_after_block(100, &default_commitment(), L1ID, L2ID)
                .await
                .unwrap();
            assert!(result.is_some());
            assert_eq!(result.unwrap().status, RequestStatus::Complete);
        }
    }

    // ==================== Housekeeping Tests ====================

    mod housekeeping {
        use super::*;

        #[tokio::test]
        async fn test_cancel_all_requests_with_statuses() {
            let db = TestDb::new().await;
            let c = db.client();

            let requests = vec![
                RequestBuilder::new().range(100, 200).status(RequestStatus::Prove).build(),
                RequestBuilder::new()
                    .range(200, 300)
                    .status(RequestStatus::WitnessGeneration)
                    .build(),
                RequestBuilder::new().range(300, 400).status(RequestStatus::Complete).build(),
            ];
            insert_requests(c, &requests).await;

            let statuses_to_cancel = [RequestStatus::Prove, RequestStatus::WitnessGeneration];
            c.cancel_all_requests_with_statuses(&statuses_to_cancel, L1ID, L2ID).await.unwrap();

            assert_eq!(count(c, RequestStatus::Cancelled).await, 2);
            assert_eq!(count(c, RequestStatus::Complete).await, 1);
            assert_eq!(count(c, RequestStatus::Prove).await, 0);
        }

        #[tokio::test]
        async fn test_delete_all_requests_with_statuses() {
            let db = TestDb::new().await;
            let c = db.client();

            let requests = vec![
                RequestBuilder::new().range(100, 200).status(RequestStatus::Failed).build(),
                RequestBuilder::new().range(200, 300).status(RequestStatus::Cancelled).build(),
                RequestBuilder::new().range(300, 400).status(RequestStatus::Complete).build(),
            ];
            insert_requests(c, &requests).await;

            let statuses_to_delete = [RequestStatus::Failed, RequestStatus::Cancelled];
            c.delete_all_requests_with_statuses(&statuses_to_delete, L1ID, L2ID).await.unwrap();

            assert_eq!(count(c, RequestStatus::Failed).await, 0);
            assert_eq!(count(c, RequestStatus::Cancelled).await, 0);
            assert_eq!(count(c, RequestStatus::Complete).await, 1);
        }

        #[tokio::test]
        async fn test_cancel_prove_requests_with_different_commitment() {
            let db = TestDb::new().await;
            let c = db.client();

            let different_commitment = B256::repeat_byte(0x99);
            let requests = vec![
                // Matching commitment - should NOT be cancelled
                RequestBuilder::new().range(100, 200).status(RequestStatus::Prove).build(),
                // Different range_vkey - should be cancelled
                RequestBuilder::new()
                    .range(200, 300)
                    .status(RequestStatus::Prove)
                    .commitment(different_commitment, B256::ZERO)
                    .build(),
            ];
            insert_requests(c, &requests).await;

            c.cancel_prove_requests_with_different_commitment_config(
                &default_commitment(),
                L1ID,
                L2ID,
            )
            .await
            .unwrap();

            assert_eq!(count(c, RequestStatus::Prove).await, 1);
            assert_eq!(count(c, RequestStatus::Cancelled).await, 0);

            // Check with the different commitment
            let diff_comm = CommitmentConfig {
                range_vkey_commitment: different_commitment,
                agg_vkey_hash: B256::ZERO,
                rollup_config_hash: B256::ZERO,
            };
            assert_eq!(
                c.fetch_request_count(RequestStatus::Cancelled, &diff_comm, L1ID, L2ID)
                    .await
                    .unwrap(),
                1
            );
        }
    }

    // ==================== Commitment Filtering Tests ====================

    #[tokio::test]
    async fn test_agg_queries_filter_by_agg_vkey_hash() {
        let db = TestDb::new().await;
        let c = db.client();

        let different_agg_vkey = B256::repeat_byte(0x42);

        let requests = vec![
            // Matching agg_vkey (B256::ZERO) - Unrequested
            RequestBuilder::new()
                .range(100, 200)
                .status(RequestStatus::Unrequested)
                .req_type(RequestType::Aggregation)
                .agg_vkey(B256::ZERO)
                .build(),
            // Different agg_vkey - Complete (for fetch_completed_agg_proof test)
            RequestBuilder::new()
                .range(100, 200)
                .status(RequestStatus::Complete)
                .req_type(RequestType::Aggregation)
                .agg_vkey(different_agg_vkey)
                .build(),
        ];
        insert_requests(c, &requests).await;

        // fetch_unrequested_agg_proof filters by agg_vkey_hash
        let result =
            c.fetch_unrequested_agg_proof(0, &default_commitment(), L1ID, L2ID).await.unwrap();
        assert!(result.is_some());

        // fetch_active_agg_proofs_count filters by agg_vkey_hash
        let count =
            c.fetch_active_agg_proofs_count(100, &default_commitment(), L1ID, L2ID).await.unwrap();
        assert_eq!(count, 1);

        // fetch_completed_agg_proof_after_block: should NOT find with default (different agg_vkey)
        let result = c
            .fetch_completed_agg_proof_after_block(100, &default_commitment(), L1ID, L2ID)
            .await
            .unwrap();
        assert!(result.is_none());

        // Query with different agg_vkey should find the Complete request
        let diff_comm = CommitmentConfig {
            range_vkey_commitment: B256::ZERO,
            agg_vkey_hash: different_agg_vkey,
            rollup_config_hash: B256::ZERO,
        };
        let result =
            c.fetch_completed_agg_proof_after_block(100, &diff_comm, L1ID, L2ID).await.unwrap();
        assert!(result.is_some());
    }
}
