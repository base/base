use alloy_primitives::{Address, B256};
use anyhow::Result;
use serde_json::Value;
use sqlx::postgres::types::PgInterval;
use sqlx::Error;
use sqlx::{postgres::PgQueryResult, PgPool};
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

    /// Fetch all requests with a specific block range and status FAILED or CANCELLED.
    ///
    /// Checks that the request has the same range vkey commitment and rollup config hash as the commitment.
    pub async fn fetch_failed_request_count_by_block_range(
        &self,
        start_block: i64,
        end_block: i64,
        l1_chain_id: i64,
        l2_chain_id: i64,
        commitment: &CommitmentConfig,
    ) -> Result<i64, Error> {
        let count = sqlx::query!(
            "SELECT COUNT(*) FROM requests WHERE start_block = $1 AND end_block = $2 AND (status = $3 OR status = $4) AND range_vkey_commitment = $5 AND rollup_config_hash = $6 AND l1_chain_id = $7 AND l2_chain_id = $8",
            start_block as i64,
            end_block as i64,
            RequestStatus::Failed as i16,
            RequestStatus::Cancelled as i16,
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

    /// Fetch all range requests that have one of the given statuses and start block >= latest_contract_l2_block.
    /// Returns only the start and end block as tuples.
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

        Ok(ranges
            .into_iter()
            .map(|r| (r.start_block, r.end_block))
            .collect())
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

    /// Get the consecutive range proofs for a given start block and end block that are complete with the same range vkey commitment.
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

    /// Fetch the checkpointed block hash and number for an aggregation request with the same start block, end block, and commitment config.
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
            (
                r.checkpointed_l1_block_hash.unwrap(),
                r.checkpointed_l1_block_number.unwrap(),
            )
        }))
    }
    /// Fetch the count of active (non-failed, non-cancelled) Aggregation proofs with the same start block, range vkey commitment, and aggregation vkey.
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
        // Note: Relayed is not included in the status list in case the user re-starts the proposer with the same DB and a different contract at the same starting block.
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

    /// Fetch the sorted list of Aggregation proofs with status Unrequested that have a start_block >= latest_contract_l2_block.
    ///
    /// Checks that the request has the same range vkey commitment, aggregation vkey, and rollup config hash as the commitment.
    ///
    /// NOTE: There should only be one "pending" Unrequested agg proof at a time for a specific start block.
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

    /// Fetch the first Range proof with status Unrequested that has a start_block >= latest_contract_l2_block.
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

    /// Fetch start and end blocks of all completed range proofs with matching range_vkey_commitment and start_block >= latest_contract_l2_block
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

        Ok(blocks
            .iter()
            .map(|block| (block.start_block, block.end_block))
            .collect())
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
