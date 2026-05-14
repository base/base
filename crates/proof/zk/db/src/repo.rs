use sqlx::{PgPool, Result, Row};
use uuid::Uuid;

use crate::{
    CreateOutboxEntry, CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome,
    CreateProofSession, MarkOutboxError, MarkOutboxProcessed, OutboxEntry, ProofRequest,
    ProofRequestListItem, ProofRequestPage, ProofSession, ProofStatus, ProofType, RetryOutcome,
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
                sequence_window, proof_type, status, prover_address, l1_head,
                intermediate_root_interval
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
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
        .bind(
            req.intermediate_root_interval
                .map(|v| {
                    i64::try_from(v).map_err(|_| {
                        sqlx::Error::Protocol("intermediate root interval exceeds i64 range".into())
                    })
                })
                .transpose()?,
        )
        .execute(&self.pool)
        .await?;

        Ok(id)
    }

    /// Atomically create a proof request and outbox entry in a transaction.
    ///
    /// On `session_id` conflict, lock the row `FOR UPDATE` and branch on state:
    /// parameter mismatch → [`CreateProofRequestError::IdCollision`];
    /// `CREATED` / `PENDING` / `RUNNING` / `SUCCEEDED` → [`CreateProofRequestOutcome::Replayed`];
    /// `FAILED` with room under `max_retries` → reset, bump `retry_count`, new outbox row
    /// ([`CreateProofRequestOutcome::Requeued`]); `FAILED` at cap → [`CreateProofRequestOutcome::RetryExhausted`].
    ///
    /// Use the same `max_retries` as [`Self::retry_or_fail_stuck_request`] (shared `retry_count` cap).
    pub async fn create_with_outbox(
        &self,
        req: CreateProofRequest,
        max_retries: i32,
    ) -> std::result::Result<CreateProofRequestOutcome, CreateProofRequestError> {
        let id = req.session_id.unwrap_or_else(Uuid::new_v4);

        let start_block_number = i64::try_from(req.start_block_number)
            .map_err(|_| sqlx::Error::Protocol("block number exceeds i64 range".into()))?;
        let number_of_blocks_to_prove = i64::try_from(req.number_of_blocks_to_prove)
            .map_err(|_| sqlx::Error::Protocol("blocks to prove exceeds i64 range".into()))?;
        let sequence_window = req
            .sequence_window
            .map(|w| {
                i64::try_from(w)
                    .map_err(|_| sqlx::Error::Protocol("sequence window exceeds i64 range".into()))
            })
            .transpose()?;
        let intermediate_root_interval = req
            .intermediate_root_interval
            .map(|v| {
                i64::try_from(v).map_err(|_| {
                    sqlx::Error::Protocol("intermediate root interval exceeds i64 range".into())
                })
            })
            .transpose()?;

        let request_params = build_outbox_params(
            start_block_number,
            number_of_blocks_to_prove,
            sequence_window,
            req.proof_type.as_str(),
            req.prover_address.as_deref(),
            req.l1_head.as_deref(),
            intermediate_root_interval,
        );

        let mut tx = self.pool.begin().await?;

        let insert_result = sqlx::query(
            r#"
            INSERT INTO proof_requests (
                id, start_block_number, number_of_blocks_to_prove,
                sequence_window, proof_type, status, prover_address, l1_head,
                intermediate_root_interval
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(id)
        .bind(start_block_number)
        .bind(number_of_blocks_to_prove)
        .bind(sequence_window)
        .bind(req.proof_type.as_str())
        .bind(ProofStatus::Created.as_str())
        .bind(&req.prover_address)
        .bind(&req.l1_head)
        .bind(intermediate_root_interval)
        .execute(&mut *tx)
        .await?;

        if insert_result.rows_affected() > 0 {
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

            tx.commit().await?;
            return Ok(CreateProofRequestOutcome::Created(id));
        }

        // Conflict path: `FOR UPDATE` serializes with stuck recovery and workers.
        let row = sqlx::query(
            r#"
            SELECT start_block_number, number_of_blocks_to_prove, sequence_window,
                   proof_type, status, prover_address, l1_head,
                   intermediate_root_interval, retry_count
            FROM proof_requests
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.rollback().await?;
            return Err(CreateProofRequestError::SessionRowMissingAfterConflict { id });
        };

        let params = CreateOutboxRequestParams {
            start_block_number,
            number_of_blocks_to_prove,
            sequence_window,
            proof_type: req.proof_type.as_str(),
            prover_address: req.prover_address.as_deref(),
            l1_head: req.l1_head.as_deref(),
            intermediate_root_interval,
        };
        if let Some(field) = params.first_mismatch(&row) {
            tx.rollback().await?;
            return Err(CreateProofRequestError::IdCollision { id, field });
        }

        let status_str: &str = row.get("status");
        let status = ProofStatus::try_from(status_str).map_err(|e| {
            sqlx::Error::Protocol(format!("Unknown proof status '{status_str}': {e}"))
        })?;

        match status {
            ProofStatus::Created
            | ProofStatus::Pending
            | ProofStatus::Running
            | ProofStatus::Succeeded => {
                tx.rollback().await?;
                Ok(CreateProofRequestOutcome::Replayed(id))
            }
            ProofStatus::Failed => {
                let retry_count: i32 = row.get("retry_count");
                if retry_count >= max_retries {
                    tx.rollback().await?;
                    return Ok(CreateProofRequestOutcome::RetryExhausted(id));
                }

                // Fail any active sessions before resetting so the requeued run cannot
                // collide with `idx_proof_sessions_request_type_active_unique`. Mirrors
                // the cleanup in `retry_or_fail_stuck_request`.
                sqlx::query(
                    r#"
                    UPDATE proof_sessions
                    SET status = $1,
                        error_message = COALESCE(error_message, $2),
                        completed_at = NOW()
                    WHERE proof_request_id = $3 AND status IN ($4, $5)
                    "#,
                )
                .bind(SessionStatus::Failed.as_str())
                .bind("cleared during create_with_outbox requeue")
                .bind(id)
                .bind(SessionStatus::Submitting.as_str())
                .bind(SessionStatus::Running.as_str())
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    UPDATE proof_requests
                    SET status = $1,
                        retry_count = retry_count + 1,
                        error_message = NULL,
                        stark_receipt = NULL,
                        snark_receipt = NULL,
                        completed_at = NULL
                    WHERE id = $2
                    "#,
                )
                .bind(ProofStatus::Created.as_str())
                .bind(id)
                .execute(&mut *tx)
                .await?;

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

                tx.commit().await?;
                Ok(CreateProofRequestOutcome::Requeued(id))
            }
        }
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
                prover_address, l1_head, intermediate_root_interval,
                created_at, updated_at, completed_at, retry_count
            FROM proof_requests
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_proof_request(&r)).transpose()
    }

    /// Update receipt fields while the request is still RUNNING.
    /// Status is kept as RUNNING — this method cannot be used for state transitions.
    /// Returns true if update succeeded, false otherwise.
    pub async fn update_receipt_if_running(&self, update: UpdateReceipt) -> Result<bool> {
        debug_assert_eq!(
            update.status,
            ProofStatus::Running,
            "update_receipt_if_running is for intermediate receipt updates only; \
             use transition_running_to_succeeded or fail_session_and_request for state transitions",
        );

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                status = 'RUNNING',
                error_message = $3,
                completed_at = NULL
            WHERE id = $4
              AND status = 'RUNNING'
            "#,
        )
        .bind(&update.stark_receipt)
        .bind(&update.snark_receipt)
        .bind(&update.error_message)
        .bind(update.id)
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

    /// Atomically create a proof session and transition proof request PENDING → RUNNING.
    /// Returns `Ok(Some(session_id))` if the request was in PENDING state.
    /// Returns `Ok(None)` if the request was NOT in PENDING state (race lost).
    pub async fn transition_pending_to_running(
        &self,
        session: CreateProofSession,
    ) -> Result<Option<i64>> {
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1
            WHERE id = $2 AND status = $3
            "#,
        )
        .bind(ProofStatus::Running.as_str())
        .bind(session.proof_request_id)
        .bind(ProofStatus::Pending.as_str())
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(None);
        }

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
        tx.commit().await?;

        Ok(Some(session_id))
    }

    /// Transition proof request PENDING → FAILED with error message.
    /// Returns true if the transition succeeded (was PENDING).
    pub async fn transition_pending_to_failed(
        &self,
        id: Uuid,
        error_message: String,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE id = $3 AND status = $4
            "#,
        )
        .bind(ProofStatus::Failed.as_str())
        .bind(&error_message)
        .bind(id)
        .bind(ProofStatus::Pending.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Transition proof request RUNNING → FAILED with optional error message.
    /// Returns true if the transition succeeded (was RUNNING).
    pub async fn transition_running_to_failed(
        &self,
        id: Uuid,
        error_message: Option<String>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE id = $3 AND status = $4
            "#,
        )
        .bind(ProofStatus::Failed.as_str())
        .bind(&error_message)
        .bind(id)
        .bind(ProofStatus::Running.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Transition proof request RUNNING → SUCCEEDED with receipt data.
    /// Returns true if the transition succeeded (was RUNNING).
    pub async fn transition_running_to_succeeded(&self, update: UpdateReceipt) -> Result<bool> {
        debug_assert_eq!(
            update.status,
            ProofStatus::Succeeded,
            "transition_running_to_succeeded called with status {:?}; the status field is ignored \
             — this method always writes SUCCEEDED",
            update.status,
        );

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                status = $3,
                error_message = $4,
                completed_at = NOW()
            WHERE id = $5 AND status = $6
            "#,
        )
        .bind(&update.stark_receipt)
        .bind(&update.snark_receipt)
        .bind(ProofStatus::Succeeded.as_str())
        .bind(&update.error_message)
        .bind(update.id)
        .bind(ProofStatus::Running.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Retry a stuck PENDING request if under the retry limit, otherwise fail it permanently.
    ///
    /// If `retry_count < max_retries`: atomically resets to CREATED, increments `retry_count`,
    /// and creates a new outbox entry so a worker picks it up again.
    /// If `retry_count >= max_retries`: transitions to FAILED.
    pub async fn retry_or_fail_stuck_request(
        &self,
        id: Uuid,
        max_retries: i32,
        error_message: &str,
    ) -> Result<RetryOutcome> {
        let mut tx = self.pool.begin().await?;

        let maybe_row = sqlx::query(
            r#"
            SELECT retry_count, status, start_block_number, number_of_blocks_to_prove,
                   sequence_window, proof_type, prover_address, l1_head,
                   intermediate_root_interval
            FROM proof_requests
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = maybe_row else {
            tx.rollback().await?;
            return Ok(RetryOutcome::Skipped);
        };

        let status_str: &str = row.get("status");
        if status_str != ProofStatus::Pending.as_str() {
            tx.rollback().await?;
            return Ok(RetryOutcome::Skipped);
        }

        let retry_count: i32 = row.get("retry_count");

        // Fail any active sessions before resetting so the retried run cannot collide with
        // `idx_proof_sessions_request_type_active_unique`. No-op on the normal reaper path,
        // since `get_stuck_requests` already excludes requests that have an active session.
        sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                error_message = COALESCE(error_message, $2),
                completed_at = NOW()
            WHERE proof_request_id = $3 AND status IN ($4, $5)
            "#,
        )
        .bind(SessionStatus::Failed.as_str())
        .bind("cleared during stuck-request retry")
        .bind(id)
        .bind(SessionStatus::Submitting.as_str())
        .bind(SessionStatus::Running.as_str())
        .execute(&mut *tx)
        .await?;

        if retry_count >= max_retries {
            sqlx::query(
                r#"
                UPDATE proof_requests
                SET status = $1,
                    error_message = $2,
                    completed_at = NOW()
                WHERE id = $3
                "#,
            )
            .bind(ProofStatus::Failed.as_str())
            .bind(format!("{error_message} (max retries exceeded after {retry_count} attempts)"))
            .bind(id)
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;
            return Ok(RetryOutcome::PermanentlyFailed);
        }

        sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1,
                retry_count = retry_count + 1,
                error_message = NULL
            WHERE id = $2
            "#,
        )
        .bind(ProofStatus::Created.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;

        // Copy the most recent outbox entry for this request. If the outbox was
        // already cleaned up (0 rows), reconstruct request_params from the
        // proof_request row we hold under FOR UPDATE.
        let outbox_copy = sqlx::query(
            r#"
            INSERT INTO proof_request_outbox (proof_request_id, request_params)
            SELECT proof_request_id, request_params
            FROM proof_request_outbox
            WHERE proof_request_id = $1
            ORDER BY sequence_id DESC
            LIMIT 1
            "#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        if outbox_copy.rows_affected() == 0 {
            let request_params = build_outbox_params(
                row.get::<i64, _>("start_block_number"),
                row.get::<i64, _>("number_of_blocks_to_prove"),
                row.get::<Option<i64>, _>("sequence_window"),
                row.get::<&str, _>("proof_type"),
                row.get::<Option<&str>, _>("prover_address"),
                row.get::<Option<&str>, _>("l1_head"),
                row.get::<Option<i64>, _>("intermediate_root_interval"),
            );

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
        }

        tx.commit().await?;
        Ok(RetryOutcome::Retried)
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

    /// Reserve a `(proof_request_id, session_type)` slot for a future backend submission.
    /// Returns `Some(reservation_id)` for the single race winner; `None` if another
    /// caller already holds an active (`SUBMITTING` or `RUNNING`) row.
    ///
    /// The row is inserted as `SUBMITTING` so sync loops (which only poll `RUNNING`
    /// rows) skip it until activation. Callers must follow up with
    /// [`Self::activate_reserved_proof_session`] on success or
    /// [`Self::fail_reserved_proof_session`] on failure.
    pub async fn reserve_proof_session(
        &self,
        proof_request_id: Uuid,
        session_type: SessionType,
    ) -> Result<Option<String>> {
        let reservation_id = format!(
            "reservation-{}-{}",
            session_type.as_str().to_ascii_lowercase(),
            Uuid::new_v4()
        );

        // ON CONFLICT predicate mirrors the partial unique index predicate.
        let row = sqlx::query(
            r#"
            INSERT INTO proof_sessions (
                proof_request_id, session_type, backend_session_id, status, metadata
            )
            VALUES ($1, $2, $3, $4, NULL)
            ON CONFLICT (proof_request_id, session_type)
                WHERE status IN ('SUBMITTING', 'RUNNING')
                DO NOTHING
            RETURNING backend_session_id
            "#,
        )
        .bind(proof_request_id)
        .bind(session_type.as_str())
        .bind(&reservation_id)
        .bind(SessionStatus::Submitting.as_str())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.get("backend_session_id")))
    }

    /// Promote a `SUBMITTING` reservation row to `RUNNING` with the real backend session
    /// id. Returns `false` if the row was no longer eligible (failed or activated
    /// out-of-band); the caller should then treat the backend job as orphaned.
    pub async fn activate_reserved_proof_session(
        &self,
        reservation_id: &str,
        session: CreateProofSession,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_sessions
            SET backend_session_id = $1,
                metadata = $2,
                status = $3,
                error_message = NULL
            WHERE backend_session_id = $4
              AND proof_request_id = $5
              AND session_type = $6
              AND status = $7
            "#,
        )
        .bind(&session.backend_session_id)
        .bind(&session.metadata)
        .bind(SessionStatus::Running.as_str())
        .bind(reservation_id)
        .bind(session.proof_request_id)
        .bind(session.session_type.as_str())
        .bind(SessionStatus::Submitting.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark a `SUBMITTING` reservation row as `FAILED` so the partial unique index
    /// releases the slot and a future poll can retry. Used when the backend submit step
    /// itself fails after a successful reservation.
    pub async fn fail_reserved_proof_session(
        &self,
        proof_request_id: Uuid,
        session_type: SessionType,
        reservation_id: &str,
        error_message: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE proof_sessions
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE backend_session_id = $3
              AND proof_request_id = $4
              AND session_type = $5
              AND status = $6
            "#,
        )
        .bind(SessionStatus::Failed.as_str())
        .bind(error_message)
        .bind(reservation_id)
        .bind(proof_request_id)
        .bind(session_type.as_str())
        .bind(SessionStatus::Submitting.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
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
                   intermediate_root_interval,
                   created_at, updated_at, completed_at, retry_count
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

    /// Get proof requests that are stuck in PENDING without a running session.
    /// These are likely orphaned due to crashes before session creation.
    /// Only checks for active (RUNNING) sessions so that retried requests
    /// with old COMPLETED/FAILED sessions are still detected as stuck.
    pub async fn get_stuck_requests(&self, stuck_timeout_mins: i32) -> Result<Vec<ProofRequest>> {
        let rows = sqlx::query(
            r#"
            SELECT
                pr.id, pr.start_block_number, pr.number_of_blocks_to_prove,
                pr.sequence_window, pr.proof_type, pr.stark_receipt, pr.snark_receipt,
                pr.status, pr.error_message, pr.prover_address, pr.l1_head,
                pr.intermediate_root_interval,
                pr.created_at, pr.updated_at, pr.completed_at, pr.retry_count
            FROM proof_requests pr
            WHERE pr.status = 'PENDING'
              AND pr.updated_at < NOW() - INTERVAL '1 minute' * $1
              AND NOT EXISTS (
                  SELECT 1 FROM proof_sessions ps
                  WHERE ps.proof_request_id = pr.id
                    AND ps.status IN ('SUBMITTING', 'RUNNING')
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

    /// Atomically update proof session to FAILED and proof request RUNNING → FAILED.
    ///
    /// The request update is guarded on `status = 'RUNNING'` so that a
    /// concurrent stuck-detector marking PENDING → FAILED cannot be
    /// overwritten. If the guard fails the entire transaction is rolled back
    /// so the session is not left in an inconsistent `FAILED` state while the
    /// request remains unchanged.
    ///
    /// Returns `true` if both updates were applied, `false` if the request was
    /// not in RUNNING state (transaction rolled back, no changes persisted).
    pub async fn fail_session_and_request(
        &self,
        backend_session_id: &str,
        proof_request_id: Uuid,
        error_message: Option<String>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET status = $1,
                error_message = $2,
                completed_at = NOW()
            WHERE id = $3
              AND status = 'RUNNING'
            "#,
        )
        .bind(ProofStatus::Failed.as_str())
        .bind(&error_message)
        .bind(proof_request_id)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }

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

        tx.commit().await?;

        Ok(true)
    }

    /// Atomically update proof session to COMPLETED and update proof request
    /// with the receipt.
    ///
    /// The request update is guarded on `status = 'RUNNING'`. If the guard
    /// fails the entire transaction is rolled back so the session is not left
    /// in an inconsistent `COMPLETED` state while the request remains
    /// unchanged.
    ///
    /// Returns `true` if both updates were applied, `false` if the request was
    /// not in RUNNING state (transaction rolled back, no changes persisted).
    pub async fn complete_session_and_update_receipt(
        &self,
        backend_session_id: &str,
        update_receipt: UpdateReceipt,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

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
              AND status = 'RUNNING'
            "#,
        )
        .bind(&update_receipt.stark_receipt)
        .bind(&update_receipt.snark_receipt)
        .bind(update_receipt.status.as_str())
        .bind(&update_receipt.error_message)
        .bind(update_receipt.id)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }

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

        tx.commit().await?;

        Ok(true)
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
                    prover_address, l1_head, intermediate_root_interval,
                    created_at, updated_at, completed_at, retry_count
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
                    prover_address, l1_head, intermediate_root_interval,
                    created_at, updated_at, completed_at, retry_count
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

    /// List proof requests with offset-based pagination and return total count.
    pub async fn list_with_offset(
        &self,
        status_filter: Option<ProofStatus>,
        page: ProofRequestPage,
    ) -> Result<(Vec<ProofRequestListItem>, u64)> {
        let (rows, count) = if let Some(status) = status_filter {
            let rows = sqlx::query_as::<_, ProofRequestListItem>(
                r#"
                SELECT
                    id, start_block_number, number_of_blocks_to_prove,
                    proof_type, status, error_message,
                    created_at, updated_at, completed_at
                FROM proof_requests
                WHERE status = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(status.as_str())
            .bind(page.limit())
            .bind(page.offset())
            .fetch_all(&self.pool);

            let count = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM proof_requests WHERE status = $1",
            )
            .bind(status.as_str())
            .fetch_one(&self.pool);

            futures::try_join!(rows, count)?
        } else {
            let rows = sqlx::query_as::<_, ProofRequestListItem>(
                r#"
                SELECT
                    id, start_block_number, number_of_blocks_to_prove,
                    proof_type, status, error_message,
                    created_at, updated_at, completed_at
                FROM proof_requests
                ORDER BY created_at DESC
                LIMIT $1 OFFSET $2
                "#,
            )
            .bind(page.limit())
            .bind(page.offset())
            .fetch_all(&self.pool);

            let count = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM proof_requests")
                .fetch_one(&self.pool);

            futures::try_join!(rows, count)?
        };

        Ok((rows, count.0.max(0) as u64))
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
        intermediate_root_interval: row.get("intermediate_root_interval"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        completed_at: row.get("completed_at"),
        retry_count: row.get("retry_count"),
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

/// Incoming fields compared to a locked `proof_requests` row for idempotency checks.
#[derive(Debug, Clone)]
struct CreateOutboxRequestParams<'a> {
    start_block_number: i64,
    number_of_blocks_to_prove: i64,
    sequence_window: Option<i64>,
    proof_type: &'a str,
    prover_address: Option<&'a str>,
    l1_head: Option<&'a str>,
    intermediate_root_interval: Option<i64>,
}

impl CreateOutboxRequestParams<'_> {
    /// First field name that disagrees with `row`, or `None`. Stable for [`CreateProofRequestError::IdCollision`].
    fn first_mismatch(&self, row: &sqlx::postgres::PgRow) -> Option<&'static str> {
        if row.get::<i64, _>("start_block_number") != self.start_block_number {
            return Some("start_block_number");
        }
        if row.get::<i64, _>("number_of_blocks_to_prove") != self.number_of_blocks_to_prove {
            return Some("number_of_blocks_to_prove");
        }
        if row.get::<Option<i64>, _>("sequence_window") != self.sequence_window {
            return Some("sequence_window");
        }
        if row.get::<&str, _>("proof_type") != self.proof_type {
            return Some("proof_type");
        }
        if row.get::<Option<&str>, _>("prover_address") != self.prover_address {
            return Some("prover_address");
        }
        if row.get::<Option<&str>, _>("l1_head") != self.l1_head {
            return Some("l1_head");
        }
        if row.get::<Option<i64>, _>("intermediate_root_interval")
            != self.intermediate_root_interval
        {
            return Some("intermediate_root_interval");
        }
        None
    }
}

/// Build the canonical JSON payload for outbox entries.
///
/// Both [`ProofRequestRepo::create_with_outbox`] and
/// [`ProofRequestRepo::retry_or_fail_stuck_request`] must produce the same
/// shape so the downstream worker can parse either identically.
fn build_outbox_params(
    start_block_number: i64,
    number_of_blocks_to_prove: i64,
    sequence_window: Option<i64>,
    proof_type: &str,
    prover_address: Option<&str>,
    l1_head: Option<&str>,
    intermediate_root_interval: Option<i64>,
) -> serde_json::Value {
    serde_json::json!({
        "start_block_number": start_block_number,
        "number_of_blocks_to_prove": number_of_blocks_to_prove,
        "sequence_window": sequence_window,
        "proof_type": proof_type,
        "prover_address": prover_address,
        "l1_head": l1_head,
        "intermediate_root_interval": intermediate_root_interval,
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
