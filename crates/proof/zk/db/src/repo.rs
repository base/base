use sqlx::{PgPool, Result, Row};
use uuid::Uuid;

use crate::{
    CreateOutboxEntry, CreateProofRequest, CreateProofSession, MarkOutboxError,
    MarkOutboxProcessed, OutboxEntry, ProofRequest, ProofSession, ProofStatus, ProofType,
    SessionStatus, SessionType, UpdateProofSession, UpdateReceipt,
};

/// Repository for proof request database operations
#[derive(Clone, Debug)]
pub struct ProofRequestRepo {
    pool: PgPool,
}

impl ProofRequestRepo {
    /// Create a new repository instance with the given database pool
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a new proof request and return its UUID
    pub async fn create(&self, req: CreateProofRequest) -> Result<Uuid> {
        let id = req.session_id.unwrap_or_else(Uuid::new_v4);

        sqlx::query(
            r#"
            INSERT INTO proof_requests (
                id, start_block_number, number_of_blocks_to_prove,
                sequence_window, proof_type, status, prover_address, l1_head
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(id)
        .bind(
            i64::try_from(req.start_block_number)
                .map_err(|_| sqlx::Error::Protocol("block number exceeds i64 range".into()))?,
        )
        .bind(
            i64::try_from(req.number_of_blocks_to_prove)
                .map_err(|_| sqlx::Error::Protocol("blocks to prove exceeds i64 range".into()))?,
        )
        .bind(
            req.sequence_window
                .map(|w| {
                    i64::try_from(w).map_err(|_| {
                        sqlx::Error::Protocol("sequence window exceeds i64 range".into())
                    })
                })
                .transpose()?,
        )
        .bind(req.proof_type.as_str())
        .bind(ProofStatus::Created.as_str())
        .bind(&req.prover_address)
        .bind(&req.l1_head)
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Atomically create a proof request and outbox entry in a transaction.
    ///
    /// When a `session_id` is provided in the request, it is used as the proof
    /// request UUID and the insert uses `ON CONFLICT (id) DO NOTHING` so that
    /// duplicate submissions return the existing request ID without error.
    pub async fn create_with_outbox(&self, req: CreateProofRequest) -> Result<Uuid> {
        let id = req.session_id.unwrap_or_else(Uuid::new_v4);

        // Serialize request params as JSON for outbox
        let request_params = serde_json::json!({
            "start_block_number": req.start_block_number,
            "number_of_blocks_to_prove": req.number_of_blocks_to_prove,
            "sequence_window": req.sequence_window,
            "proof_type": req.proof_type.as_str(),
            "prover_address": req.prover_address,
            "l1_head": req.l1_head,
        });

        // Start transaction
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            r#"
            INSERT INTO proof_requests (
                id, start_block_number, number_of_blocks_to_prove,
                sequence_window, proof_type, status, prover_address, l1_head
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(id)
        .bind(
            i64::try_from(req.start_block_number)
                .map_err(|_| sqlx::Error::Protocol("block number exceeds i64 range".into()))?,
        )
        .bind(
            i64::try_from(req.number_of_blocks_to_prove)
                .map_err(|_| sqlx::Error::Protocol("blocks to prove exceeds i64 range".into()))?,
        )
        .bind(
            req.sequence_window
                .map(|w| {
                    i64::try_from(w).map_err(|_| {
                        sqlx::Error::Protocol("sequence window exceeds i64 range".into())
                    })
                })
                .transpose()?,
        )
        .bind(req.proof_type.as_str())
        .bind(ProofStatus::Created.as_str())
        .bind(&req.prover_address)
        .bind(&req.l1_head)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(id);
        }

        sqlx::query(
            r#"
            INSERT INTO proof_request_outbox (proof_request_id, request_params)
            VALUES ($1, $2)
            "#,
        )
        .bind(id)
        .bind(&request_params)
        .execute(&mut *tx)
        .await?;

        // Commit transaction
        tx.commit().await?;

        Ok(id)
    }

    /// Get a proof request by ID
    pub async fn get(&self, id: Uuid) -> Result<Option<ProofRequest>> {
        let row = sqlx::query(
            r#"
            SELECT
                id, start_block_number, number_of_blocks_to_prove,
                sequence_window, proof_type,
                stark_receipt, snark_receipt,
                status, error_message,
                prover_address, l1_head,
                created_at, updated_at, completed_at
            FROM proof_requests
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_proof_request(&r)).transpose()
    }

    /// Update a proof request with receipt
    pub async fn update_receipt(&self, update: UpdateReceipt) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                status = $3,
                error_message = $4,
                completed_at = CASE WHEN $3 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $5
            "#,
        )
        .bind(&update.stark_receipt)
        .bind(&update.snark_receipt)
        .bind(update.status.as_str())
        .bind(&update.error_message)
        .bind(update.id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update status and error message
    pub async fn update_status(
        &self,
        id: Uuid,
        status: ProofStatus,
        error_message: Option<String>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                status = $1,
                error_message = $2,
                completed_at = CASE WHEN $1 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $3
            "#,
        )
        .bind(status.as_str())
        .bind(&error_message)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update receipt only if current status is non-terminal (PENDING/RUNNING).
    /// Returns true if update succeeded, false if already terminal.
    pub async fn update_receipt_if_non_terminal(&self, update: UpdateReceipt) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                status = $3,
                error_message = $4,
                completed_at = CASE WHEN $3 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $5
              AND status NOT IN ('SUCCEEDED', 'FAILED')
            "#,
        )
        .bind(&update.stark_receipt)
        .bind(&update.snark_receipt)
        .bind(update.status.as_str())
        .bind(&update.error_message)
        .bind(update.id)
        .execute(&self.pool)
        .await?;

        let updated = result.rows_affected() > 0;

        Ok(updated)
    }

    /// Update status only if current status is non-terminal.
    /// Returns true if update succeeded, false if already terminal.
    pub async fn update_status_if_non_terminal(
        &self,
        id: Uuid,
        status: ProofStatus,
        error_message: Option<String>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                status = $1,
                error_message = $2,
                completed_at = CASE WHEN $1 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $3
              AND status NOT IN ('SUCCEEDED', 'FAILED')
            "#,
        )
        .bind(status.as_str())
        .bind(&error_message)
        .bind(id)
        .execute(&self.pool)
        .await?;

        let updated = result.rows_affected() > 0;

        Ok(updated)
    }

    /// Atomically claim a task by transitioning it from CREATED to PENDING.
    /// Returns true if the task was successfully claimed (was in CREATED state).
    /// Returns false if the task was already claimed or doesn't exist.
    pub async fn atomic_claim_task(&self, id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1
            WHERE id = $2 AND status = $3
            "#,
        )
        .bind(ProofStatus::Pending.as_str())
        .bind(id)
        .bind(ProofStatus::Created.as_str())
        .execute(&self.pool)
        .await?;

        let claimed = result.rows_affected() > 0;

        Ok(claimed)
    }

    // ========== Proof Session Methods ==========

    /// Create a new proof session
    pub async fn create_proof_session(&self, session: CreateProofSession) -> Result<i64> {
        let row = sqlx::query(
            r#"
            INSERT INTO proof_sessions (
                proof_request_id, session_type, backend_session_id, status, metadata
            )
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#,
        )
        .bind(session.proof_request_id)
        .bind(session.session_type.as_str())
        .bind(&session.backend_session_id)
        .bind(SessionStatus::Running.as_str())
        .bind(&session.metadata)
        .fetch_one(&self.pool)
        .await?;

        let id: i64 = row.get("id");
        Ok(id)
    }

    /// Get a proof session by backend session ID
    pub async fn get_session_by_backend_id(
        &self,
        backend_session_id: &str,
    ) -> Result<Option<ProofSession>> {
        let row = sqlx::query(
            r#"
            SELECT id, proof_request_id, session_type, backend_session_id,
                   status, error_message, metadata, created_at, completed_at
            FROM proof_sessions
            WHERE backend_session_id = $1
            "#,
        )
        .bind(backend_session_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_proof_session(&r)).transpose()
    }

    /// Get all sessions for a proof request
    pub async fn get_sessions_for_request(
        &self,
        proof_request_id: Uuid,
    ) -> Result<Vec<ProofSession>> {
        let rows = sqlx::query(
            r#"
            SELECT id, proof_request_id, session_type, backend_session_id,
                   status, error_message, metadata, created_at, completed_at
            FROM proof_sessions
            WHERE proof_request_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(proof_request_id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_proof_session).collect()
    }

    /// Get all running sessions (for polling)
    pub async fn get_running_sessions(&self) -> Result<Vec<ProofSession>> {
        let rows = sqlx::query(
            r#"
            SELECT id, proof_request_id, session_type, backend_session_id,
                   status, error_message, metadata, created_at, completed_at
            FROM proof_sessions
            WHERE status = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(SessionStatus::Running.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_proof_session).collect()
    }

    /// Get all running proof requests (for polling)
    pub async fn get_running_proof_requests(&self) -> Result<Vec<ProofRequest>> {
        let rows = sqlx::query(
            r#"
            SELECT id, start_block_number, number_of_blocks_to_prove,
                   sequence_window, proof_type, stark_receipt, snark_receipt,
                   status, error_message, prover_address, l1_head,
                   created_at, updated_at, completed_at
            FROM proof_requests
            WHERE status = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(ProofStatus::Running.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_proof_request).collect()
    }

    /// Get proof requests that are stuck in PENDING without any sessions.
    /// These are likely orphaned due to crashes before session creation.
    pub async fn get_stuck_requests(&self, stuck_timeout_mins: i32) -> Result<Vec<ProofRequest>> {
        let rows = sqlx::query(
            r#"
            SELECT
                pr.id, pr.start_block_number, pr.number_of_blocks_to_prove,
                pr.sequence_window, pr.proof_type, pr.stark_receipt, pr.snark_receipt,
                pr.status, pr.error_message, pr.prover_address, pr.l1_head,
                pr.created_at, pr.updated_at, pr.completed_at
            FROM proof_requests pr
            WHERE pr.status = 'PENDING'
              AND pr.updated_at < NOW() - INTERVAL '1 minute' * $1
              AND NOT EXISTS (
                  SELECT 1 FROM proof_sessions ps
                  WHERE ps.proof_request_id = pr.id
              )
            ORDER BY pr.created_at ASC
            "#,
        )
        .bind(stuck_timeout_mins)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(row_to_proof_request).collect()
    }

    /// Update a proof session status
    pub async fn update_proof_session(&self, update: UpdateProofSession) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                error_message = $2,
                metadata = COALESCE($3, metadata),
                completed_at = CASE WHEN $1 IN ('COMPLETED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE backend_session_id = $4
            "#,
        )
        .bind(update.status.as_str())
        .bind(&update.error_message)
        .bind(&update.metadata)
        .bind(&update.backend_session_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update a proof session only if it's still in RUNNING state (non-terminal).
    /// Returns true if the session was updated, false if already terminal.
    pub async fn update_proof_session_if_non_terminal(
        &self,
        update: UpdateProofSession,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                error_message = $2,
                metadata = COALESCE($3, metadata),
                completed_at = CASE WHEN $1 IN ('COMPLETED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE backend_session_id = $4
              AND status = 'RUNNING'
            "#,
        )
        .bind(update.status.as_str())
        .bind(&update.error_message)
        .bind(&update.metadata)
        .bind(&update.backend_session_id)
        .execute(&self.pool)
        .await?;

        let updated = result.rows_affected() > 0;
        Ok(updated)
    }

    /// Atomically create a proof session and update proof request to RUNNING
    pub async fn create_session_and_update_status(
        &self,
        session: CreateProofSession,
        status: ProofStatus,
    ) -> Result<i64> {
        let mut tx = self.pool.begin().await?;

        // Insert proof session
        let row = sqlx::query(
            r#"
            INSERT INTO proof_sessions (
                proof_request_id, session_type, backend_session_id, status, metadata
            )
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#,
        )
        .bind(session.proof_request_id)
        .bind(session.session_type.as_str())
        .bind(&session.backend_session_id)
        .bind(SessionStatus::Running.as_str())
        .bind(&session.metadata)
        .fetch_one(&mut *tx)
        .await?;

        let session_id: i64 = row.get("id");

        // Update proof request status
        sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1
            WHERE id = $2
            "#,
        )
        .bind(status.as_str())
        .bind(session.proof_request_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(session_id)
    }

    /// Atomically update proof session to FAILED and proof request to FAILED
    pub async fn fail_session_and_request(
        &self,
        backend_session_id: &str,
        proof_request_id: Uuid,
        error_message: Option<String>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        // Update proof session to FAILED
        sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE backend_session_id = $3
            "#,
        )
        .bind(SessionStatus::Failed.as_str())
        .bind(&error_message)
        .bind(backend_session_id)
        .execute(&mut *tx)
        .await?;

        // Update proof request to FAILED (only if not already terminal)
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE id = $3
              AND status NOT IN ('SUCCEEDED', 'FAILED')
            "#,
        )
        .bind(ProofStatus::Failed.as_str())
        .bind(&error_message)
        .bind(proof_request_id)
        .execute(&mut *tx)
        .await?;

        let updated = result.rows_affected() > 0;

        tx.commit().await?;

        Ok(updated)
    }

    /// Atomically update proof session to COMPLETED and update proof request with receipt
    pub async fn complete_session_and_update_receipt(
        &self,
        backend_session_id: &str,
        update_receipt: UpdateReceipt,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        // Update proof session to COMPLETED
        sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                completed_at = NOW()
            WHERE backend_session_id = $2
            "#,
        )
        .bind(SessionStatus::Completed.as_str())
        .bind(backend_session_id)
        .execute(&mut *tx)
        .await?;

        // Update proof request with receipt (only if not already terminal)
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                status = $3,
                error_message = $4,
                completed_at = CASE WHEN $3 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $5
              AND status NOT IN ('SUCCEEDED', 'FAILED')
            "#,
        )
        .bind(&update_receipt.stark_receipt)
        .bind(&update_receipt.snark_receipt)
        .bind(update_receipt.status.as_str())
        .bind(&update_receipt.error_message)
        .bind(update_receipt.id)
        .execute(&mut *tx)
        .await?;

        let updated = result.rows_affected() > 0;

        tx.commit().await?;

        Ok(updated)
    }

    /// List all proof requests with optional status filter
    pub async fn list(
        &self,
        status_filter: Option<ProofStatus>,
        limit: i64,
    ) -> Result<Vec<ProofRequest>> {
        let rows = if let Some(status) = status_filter {
            sqlx::query(
                r#"
                SELECT
                    id, start_block_number, number_of_blocks_to_prove,
                    sequence_window, proof_type,
                    stark_receipt, snark_receipt,
                    status, error_message,
                    prover_address, l1_head,
                    created_at, updated_at, completed_at
                FROM proof_requests
                WHERE status = $1
                ORDER BY created_at DESC
                LIMIT $2
                "#,
            )
            .bind(status.as_str())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT
                    id, start_block_number, number_of_blocks_to_prove,
                    sequence_window, proof_type,
                    stark_receipt, snark_receipt,
                    status, error_message,
                    prover_address, l1_head,
                    created_at, updated_at, completed_at
                FROM proof_requests
                ORDER BY created_at DESC
                LIMIT $1
                "#,
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        rows.iter().map(row_to_proof_request).collect()
    }

    // ========== Outbox Methods ==========

    /// Create an outbox entry for background task processing.
    /// This should be called in the same transaction as creating the proof request.
    pub async fn create_outbox_entry(&self, entry: CreateOutboxEntry) -> Result<i64> {
        let row = sqlx::query(
            r#"
            INSERT INTO proof_request_outbox (proof_request_id, request_params)
            VALUES ($1, $2)
            RETURNING sequence_id
            "#,
        )
        .bind(entry.proof_request_id)
        .bind(&entry.request_params)
        .fetch_one(&self.pool)
        .await?;

        let sequence_id: i64 = row.get("sequence_id");

        Ok(sequence_id)
    }

    /// Get the next batch of unprocessed outbox entries.
    ///
    /// Returns entries in order by `sequence_id` (FIFO), excluding entries that
    /// have exceeded `max_retries` attempts.
    pub async fn get_unprocessed_outbox_entries(
        &self,
        limit: i64,
        max_retries: i32,
    ) -> Result<Vec<OutboxEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT sequence_id, proof_request_id, request_params,
                   processed, processed_at, retry_count, last_error, created_at
            FROM proof_request_outbox
            WHERE processed = FALSE
              AND retry_count < $2
            ORDER BY sequence_id ASC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .bind(max_retries)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_outbox_entry).collect())
    }

    /// Mark an outbox entry as processed
    pub async fn mark_outbox_processed(&self, mark: MarkOutboxProcessed) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE proof_request_outbox
            SET processed = TRUE,
                processed_at = NOW()
            WHERE sequence_id = $1
            "#,
        )
        .bind(mark.sequence_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Record an error for an outbox entry and increment retry count
    pub async fn mark_outbox_error(&self, mark: MarkOutboxError) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE proof_request_outbox
            SET retry_count = retry_count + 1,
                last_error = $1
            WHERE sequence_id = $2
            "#,
        )
        .bind(&mark.error_message)
        .bind(mark.sequence_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete old processed outbox entries (for cleanup)
    pub async fn delete_old_processed_outbox_entries(&self, older_than_days: i32) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM proof_request_outbox
            WHERE processed = TRUE
              AND processed_at < NOW() - INTERVAL '1 day' * $1
            "#,
        )
        .bind(older_than_days)
        .execute(&self.pool)
        .await?;

        let rows_deleted = result.rows_affected();

        Ok(rows_deleted)
    }
}

/// Helper function to convert a database row to `ProofRequest`
fn row_to_proof_request(row: &sqlx::postgres::PgRow) -> Result<ProofRequest> {
    let status_str: &str = row.get("status");
    let status = ProofStatus::try_from(status_str)
        .map_err(|e| sqlx::Error::Protocol(format!("Unknown proof status '{status_str}': {e}")))?;

    let proof_type_str: &str = row.get("proof_type");
    let proof_type = ProofType::try_from(proof_type_str).map_err(|e| {
        sqlx::Error::Protocol(format!("Unknown proof_type '{proof_type_str}': {e}"))
    })?;

    Ok(ProofRequest {
        id: row.get("id"),
        start_block_number: row.get("start_block_number"),
        number_of_blocks_to_prove: row.get("number_of_blocks_to_prove"),
        sequence_window: row.get("sequence_window"),
        proof_type,
        stark_receipt: row.get("stark_receipt"),
        snark_receipt: row.get("snark_receipt"),
        status,
        error_message: row.get("error_message"),
        prover_address: row.get("prover_address"),
        l1_head: row.get("l1_head"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        completed_at: row.get("completed_at"),
    })
}

/// Helper function to convert a database row to `ProofSession`
fn row_to_proof_session(row: &sqlx::postgres::PgRow) -> Result<ProofSession> {
    let status_str: &str = row.get("status");
    let status = SessionStatus::try_from(status_str).map_err(|e| {
        sqlx::Error::Protocol(format!("Unknown session status '{status_str}': {e}"))
    })?;

    let session_type_str: &str = row.get("session_type");
    let session_type = SessionType::try_from(session_type_str).map_err(|e| {
        sqlx::Error::Protocol(format!("Unknown session type '{session_type_str}': {e}"))
    })?;

    Ok(ProofSession {
        id: row.get("id"),
        proof_request_id: row.get("proof_request_id"),
        session_type,
        backend_session_id: row.get("backend_session_id"),
        status,
        error_message: row.get("error_message"),
        metadata: row.get("metadata"),
        created_at: row.get("created_at"),
        completed_at: row.get("completed_at"),
    })
}

/// Helper function to convert a database row to `OutboxEntry`
fn row_to_outbox_entry(row: &sqlx::postgres::PgRow) -> OutboxEntry {
    OutboxEntry {
        sequence_id: row.get("sequence_id"),
        proof_request_id: row.get("proof_request_id"),
        request_params: row.get("request_params"),
        processed: row.get("processed"),
        processed_at: row.get("processed_at"),
        retry_count: row.get("retry_count"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
    }
}
