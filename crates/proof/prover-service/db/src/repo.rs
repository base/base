use base_prover_service_protocol::{
    ProofResult as ProtocolProofResult, SnarkGroth16ProofResult, ZkProofResult, ZkVm,
};
use chrono::Utc;
use sqlx::{PgPool, Result, Row};
use uuid::Uuid;

use crate::{
    ApiProofType, ClaimProofJob, CompleteClaimedProofJob, CompleteProofResult, CreateProofRequest,
    CreateProofRequestError, CreateProofRequestOutcome, CreateProofRequestValidationError,
    CreateProofSession, FailExpiredProofJobs, HeartbeatOutcome, HeartbeatProofJob, ProofJob,
    ProofJobStatus, ProofRequest, ProofRequestListItem, ProofRequestPage, ProofSession,
    ProofStatus, ProofType, RetryOutcome, SessionStatus, SessionType, SubmitProofOutcome, TeeKind,
    UpdateProofSession, UpdateReceipt, ZkVmKind, canonical_session_id,
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
    pub async fn create(
        &self,
        req: CreateProofRequest,
    ) -> std::result::Result<Uuid, CreateProofRequestError> {
        let prepared = PreparedProofRequest::try_from(req)?;

        sqlx::query(
            r#"
            INSERT INTO proof_requests (
                id, session_id, request_payload, api_proof_type, zk_vm, tee_kind, start_block_number,
                number_of_blocks_to_prove, sequence_window, proof_type, status,
                prover_address, l1_head, intermediate_root_interval
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            "#,
        )
        .bind(prepared.id)
        .bind(&prepared.session_id)
        .bind(&prepared.request_payload)
        .bind(prepared.api_proof_type.as_str())
        .bind(prepared.zk_vm.map(|zk_vm| zk_vm.as_str()))
        .bind(prepared.tee_kind.map(|tee_kind| tee_kind.as_str()))
        .bind(prepared.start_block_number)
        .bind(prepared.number_of_blocks_to_prove)
        .bind(prepared.sequence_window)
        .bind(prepared.proof_type.map(|proof_type| proof_type.as_str()))
        .bind(ProofStatus::Created.as_str())
        .bind(&prepared.prover_address)
        .bind(&prepared.l1_head)
        .bind(prepared.intermediate_root_interval)
        .execute(&self.pool)
        .await?;

        Ok(prepared.id)
    }

    /// Atomically create a proof request for the worker API queue.
    ///
    /// New requests are inserted into `proof_requests` with `job_status = 'PENDING'`
    /// by the schema default. External workers claim these rows via
    /// [`Self::claim_next_proof_job`].
    ///
    /// On `session_id` conflict, lock the row `FOR UPDATE` and branch on state:
    /// parameter mismatch -> [`CreateProofRequestError::IdCollision`];
    /// `CREATED` / `PENDING` / `RUNNING` / `SUCCEEDED` -> [`CreateProofRequestOutcome::Replayed`];
    /// `FAILED` with room under `max_retries` -> reset, bump `retry_count`,
    /// and make the job claimable again ([`CreateProofRequestOutcome::Requeued`]);
    /// `FAILED` at cap -> [`CreateProofRequestOutcome::RetryExhausted`].
    pub async fn create_for_worker_queue(
        &self,
        req: CreateProofRequest,
        max_retries: i32,
    ) -> std::result::Result<CreateProofRequestOutcome, CreateProofRequestError> {
        let prepared = PreparedProofRequest::try_from(req)?;
        let mut tx = self.pool.begin().await?;

        let insert_result = sqlx::query(
            r#"
            INSERT INTO proof_requests (
                id, session_id, request_payload, api_proof_type, zk_vm, tee_kind, start_block_number,
                number_of_blocks_to_prove, sequence_window, proof_type, status,
                prover_address, l1_head, intermediate_root_interval
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            ON CONFLICT ((COALESCE(session_id, id::text))) DO NOTHING
            "#,
        )
        .bind(prepared.id)
        .bind(&prepared.session_id)
        .bind(&prepared.request_payload)
        .bind(prepared.api_proof_type.as_str())
        .bind(prepared.zk_vm.map(|zk_vm| zk_vm.as_str()))
        .bind(prepared.tee_kind.map(|tee_kind| tee_kind.as_str()))
        .bind(prepared.start_block_number)
        .bind(prepared.number_of_blocks_to_prove)
        .bind(prepared.sequence_window)
        .bind(prepared.proof_type.map(|proof_type| proof_type.as_str()))
        .bind(ProofStatus::Created.as_str())
        .bind(&prepared.prover_address)
        .bind(&prepared.l1_head)
        .bind(prepared.intermediate_root_interval)
        .execute(&mut *tx)
        .await?;

        if insert_result.rows_affected() > 0 {
            tx.commit().await?;
            return Ok(CreateProofRequestOutcome::Created(prepared.id));
        }

        // Conflict path: `FOR UPDATE` serializes with retries and workers.
        let row = sqlx::query(
            r#"
            SELECT id, COALESCE(session_id, id::text) AS session_id,
                   request_payload, api_proof_type, zk_vm, tee_kind,
                   start_block_number, number_of_blocks_to_prove, sequence_window,
                   proof_type, status, prover_address, l1_head,
                   intermediate_root_interval, retry_count
            FROM proof_requests
            WHERE COALESCE(session_id, id::text) = $1
            FOR UPDATE
            "#,
        )
        .bind(&prepared.session_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.rollback().await?;
            return Err(CreateProofRequestError::SessionRowMissingAfterConflict {
                id: prepared.id,
            });
        };

        let existing_id: Uuid = row.get("id");
        let params = CreateRequestParams {
            request_payload: &prepared.request_payload,
            api_proof_type: prepared.api_proof_type.as_str(),
            zk_vm: prepared.zk_vm.map(|zk_vm| zk_vm.as_str()),
            tee_kind: prepared.tee_kind.map(|tee_kind| tee_kind.as_str()),
            start_block_number: prepared.start_block_number,
            number_of_blocks_to_prove: prepared.number_of_blocks_to_prove,
            sequence_window: prepared.sequence_window,
            proof_type: prepared.proof_type.map(|proof_type| proof_type.as_str()),
            prover_address: prepared.prover_address.as_deref(),
            l1_head: prepared.l1_head.as_deref(),
            intermediate_root_interval: prepared.intermediate_root_interval,
        };
        if let Some(field) = params.first_mismatch(&row) {
            tx.rollback().await?;
            return Err(CreateProofRequestError::IdCollision { id: existing_id, field });
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
                Ok(CreateProofRequestOutcome::Replayed(existing_id))
            }
            ProofStatus::Failed => {
                let retry_count: i32 = row.get("retry_count");
                if retry_count >= max_retries {
                    tx.rollback().await?;
                    return Ok(CreateProofRequestOutcome::RetryExhausted(existing_id));
                }

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
                .bind("cleared during worker-queue requeue")
                .bind(existing_id)
                .bind(SessionStatus::Submitting.as_str())
                .bind(SessionStatus::Running.as_str())
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    UPDATE proof_requests
                    SET status = $1,
                        job_status = 'PENDING',
                        retry_count = retry_count + 1,
                        error_message = NULL,
                        stark_receipt = NULL,
                        snark_receipt = NULL,
                        result_payload = NULL,
                        submitted_by_worker_id = NULL,
                        submitted_lock_id = NULL,
                        completed_at = NULL,
                        worker_id = NULL,
                        lock_id = NULL,
                        lock_expires_at = NULL,
                        claimed_at = NULL,
                        last_heartbeat_at = NULL,
                        attempt = 0
                    WHERE id = $2
                    "#,
                )
                .bind(ProofStatus::Created.as_str())
                .bind(existing_id)
                .execute(&mut *tx)
                .await?;

                tx.commit().await?;
                Ok(CreateProofRequestOutcome::Requeued(existing_id))
            }
        }
    }

    /// Get a proof request by ID
    pub async fn get(&self, id: Uuid) -> Result<Option<ProofRequest>> {
        let row = sqlx::query(
            r#"
            SELECT
                id, COALESCE(session_id, id::text) AS session_id,
                request_payload, api_proof_type, zk_vm, tee_kind,
                start_block_number, number_of_blocks_to_prove, sequence_window, proof_type,
                stark_receipt, snark_receipt, result_payload,
                submitted_by_worker_id, submitted_lock_id,
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

    /// Get a proof request by public protocol session ID.
    pub async fn get_by_session_id(&self, session_id: &str) -> Result<Option<ProofRequest>> {
        let session_id = canonical_session_id(session_id)
            .map_err(|e| sqlx::Error::InvalidArgument(e.to_string()))?;
        let row = sqlx::query(
            r#"
            SELECT
                id, COALESCE(session_id, id::text) AS session_id,
                request_payload, api_proof_type, zk_vm, tee_kind,
                start_block_number, number_of_blocks_to_prove, sequence_window, proof_type,
                stark_receipt, snark_receipt, result_payload,
                submitted_by_worker_id, submitted_lock_id,
                status, error_message,
                prover_address, l1_head, intermediate_root_interval,
                created_at, updated_at, completed_at, retry_count
            FROM proof_requests
            WHERE COALESCE(session_id, id::text) = $1
            "#,
        )
        .bind(&session_id)
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

        let result_payload = result_payload_from_receipt_update(&update)?;

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                result_payload = COALESCE($3, result_payload),
                status = $4,
                error_message = $5,
                completed_at = NOW()
            WHERE id = $6 AND status = $7
            "#,
        )
        .bind(&update.stark_receipt)
        .bind(&update.snark_receipt)
        .bind(&result_payload)
        .bind(ProofStatus::Succeeded.as_str())
        .bind(&update.error_message)
        .bind(update.id)
        .bind(ProofStatus::Running.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Transition proof request RUNNING → SUCCEEDED with a protocol-native result payload.
    ///
    /// ZK results are also mirrored into `stark_receipt` or `snark_receipt` for legacy
    /// compatibility. TEE results are stored only in `result_payload`.
    pub async fn complete_running_proof_result(&self, update: CompleteProofResult) -> Result<bool> {
        let result_payload =
            serde_json::to_value(&update.result).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let (stark_receipt, snark_receipt) = compatibility_receipts_for_result(&update.result);

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET result_payload = $1,
                submitted_by_worker_id = $2,
                submitted_lock_id = $3,
                stark_receipt = COALESCE($4, stark_receipt),
                snark_receipt = COALESCE($5, snark_receipt),
                status = $6,
                error_message = $7,
                completed_at = NOW()
            WHERE id = $8 AND status = $9
            "#,
        )
        .bind(&result_payload)
        .bind(&update.submitted_by_worker_id)
        .bind(&update.submitted_lock_id)
        .bind(&stark_receipt)
        .bind(&snark_receipt)
        .bind(ProofStatus::Succeeded.as_str())
        .bind(&update.error_message)
        .bind(update.id)
        .bind(ProofStatus::Running.as_str())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    // ========== Worker Job API Methods ==========

    /// Atomically claim the next eligible worker proof job (`getNextProof`).
    ///
    /// Selects the lowest-start-block job whose `api_proof_type` matches the
    /// worker and whose capability discriminator (`tee_kind` for TEE, `zk_vm` for
    /// ZK) is in the worker's advertised set. A job is claimable when it is
    /// `PENDING`, or when it is `CLAIMED` with an expired lock and still under the
    /// reclaim budget (`attempt < max_attempts`). The row is locked with
    /// `FOR UPDATE SKIP LOCKED` so concurrent workers never claim the same job.
    ///
    /// On success the job transitions to `job_status = 'CLAIMED'` and requester
    /// `status = 'RUNNING'`, with a freshly rotated `lock_id`, incremented
    /// `attempt`, and an extended `lock_expires_at`. Returns `None` when no job is
    /// eligible (including when the worker advertises no matching capabilities).
    pub async fn claim_next_proof_job(&self, req: ClaimProofJob) -> Result<Option<ProofJob>> {
        let lock_id = Uuid::new_v4();
        let sql = claim_query(req.api_proof_type);
        let cap_values = worker_capability_values(&req);

        let row = sqlx::query(&sql)
            .bind(&req.worker_id)
            .bind(lock_id)
            .bind(i64::from(req.lock_duration_seconds))
            .bind(req.api_proof_type.as_str())
            .bind(&cap_values)
            .bind(i64::from(req.max_attempts))
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref().map(row_to_proof_job).transpose()
    }

    /// Fetch a worker-visible proof job by protocol `session_id`.
    pub async fn get_proof_job_by_session_id(&self, session_id: &str) -> Result<Option<ProofJob>> {
        let session_id = canonical_session_id(session_id)
            .map_err(|e| sqlx::Error::InvalidArgument(e.to_string()))?;
        self.get_proof_job_by_canonical_session_id(&session_id).await
    }

    async fn get_proof_job_by_canonical_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ProofJob>> {
        let columns = PROOF_JOB_RETURNING_COLUMNS;
        let sql = format!(
            r#"
            SELECT {columns}
            FROM proof_requests
            WHERE COALESCE(session_id, id::text) = $1
            "#
        );

        let row = sqlx::query(&sql).bind(session_id).fetch_optional(&self.pool).await?;

        row.as_ref().map(row_to_proof_job).transpose()
    }

    /// Extend the lock for the currently owned worker proof job (`heartbeat`).
    ///
    /// The update is guarded by `session_id`, `job_status = 'CLAIMED'`, `lock_id`,
    /// `worker_id`, and an unexpired `lock_expires_at`. A stale worker cannot
    /// revive an expired or reclaimed job because the update only succeeds for the
    /// current fencing token.
    pub async fn heartbeat_proof_job(&self, req: HeartbeatProofJob) -> Result<HeartbeatOutcome> {
        let session_id = canonical_session_id(&req.session_id)
            .map_err(|e| sqlx::Error::InvalidArgument(e.to_string()))?;
        let columns = PROOF_JOB_RETURNING_COLUMNS;
        let sql = format!(
            r#"
            UPDATE proof_requests
            SET last_heartbeat_at = NOW(),
                lock_expires_at = NOW() + ($4)::double precision * INTERVAL '1 second'
            WHERE COALESCE(session_id, id::text) = $1
              AND job_status = 'CLAIMED'
              AND lock_id = $2
              AND worker_id = $3
              AND lock_expires_at > NOW()
            RETURNING {columns}
            "#
        );

        let row = sqlx::query(&sql)
            .bind(&session_id)
            .bind(req.lock_id)
            .bind(&req.worker_id)
            .bind(i64::from(req.lock_duration_seconds))
            .fetch_optional(&self.pool)
            .await?;

        if let Some(row) = row {
            return row_to_proof_job(&row).map(HeartbeatOutcome::Updated);
        }

        let Some(job) = self.get_proof_job_by_canonical_session_id(&session_id).await? else {
            return Ok(HeartbeatOutcome::NotFound);
        };

        if matches!(job.job_status, ProofJobStatus::Succeeded | ProofJobStatus::Failed) {
            return Ok(HeartbeatOutcome::Terminal(job));
        }
        if job.job_status != ProofJobStatus::Claimed {
            return Ok(HeartbeatOutcome::NotClaimed(job));
        }
        if job.lock_id != Some(req.lock_id) || job.worker_id.as_deref() != Some(&req.worker_id) {
            return Ok(HeartbeatOutcome::StaleLock(job));
        }
        if job.lock_expires_at.is_none_or(|expires_at| expires_at <= Utc::now()) {
            return Ok(HeartbeatOutcome::Expired(job));
        }

        Ok(HeartbeatOutcome::Unknown(job))
    }

    /// Complete the currently owned worker proof job (`submitProof`).
    ///
    /// The ownership guard matches [`Self::heartbeat_proof_job`]. On success the
    /// worker job and requester proof both transition to `SUCCEEDED`,
    /// `result_payload` stores the protocol result, and ZK results are mirrored
    /// into legacy receipt columns for compatibility.
    pub async fn complete_claimed_proof_job(
        &self,
        req: CompleteClaimedProofJob,
    ) -> Result<SubmitProofOutcome> {
        let session_id = canonical_session_id(&req.session_id)
            .map_err(|e| sqlx::Error::InvalidArgument(e.to_string()))?;
        let result_payload =
            serde_json::to_value(&req.result).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        let (stark_receipt, snark_receipt) = compatibility_receipts_for_result(&req.result);
        let submitted_lock_id = req.lock_id.to_string();
        let columns = PROOF_JOB_RETURNING_COLUMNS;
        let sql = format!(
            r#"
            UPDATE proof_requests
            SET job_status = 'SUCCEEDED',
                status = 'SUCCEEDED',
                result_payload = $4,
                submitted_by_worker_id = $3,
                submitted_lock_id = $5,
                stark_receipt = COALESCE($6, stark_receipt),
                snark_receipt = COALESCE($7, snark_receipt),
                error_message = NULL,
                completed_at = NOW()
            WHERE COALESCE(session_id, id::text) = $1
              AND job_status = 'CLAIMED'
              AND lock_id = $2
              AND worker_id = $3
              AND lock_expires_at > NOW()
            RETURNING {columns}
            "#
        );

        let row = sqlx::query(&sql)
            .bind(&session_id)
            .bind(req.lock_id)
            .bind(&req.worker_id)
            .bind(&result_payload)
            .bind(&submitted_lock_id)
            .bind(&stark_receipt)
            .bind(&snark_receipt)
            .fetch_optional(&self.pool)
            .await?;

        if let Some(row) = row {
            return row_to_proof_job(&row).map(SubmitProofOutcome::Completed);
        }

        let Some(job) = self.get_proof_job_by_canonical_session_id(&session_id).await? else {
            return Ok(SubmitProofOutcome::NotFound);
        };

        if matches!(job.job_status, ProofJobStatus::Succeeded | ProofJobStatus::Failed) {
            return Ok(SubmitProofOutcome::Terminal(job));
        }
        if job.job_status != ProofJobStatus::Claimed {
            return Ok(SubmitProofOutcome::NotClaimed(job));
        }
        if job.lock_id != Some(req.lock_id) || job.worker_id.as_deref() != Some(&req.worker_id) {
            return Ok(SubmitProofOutcome::StaleLock(job));
        }
        if job.lock_expires_at.is_none_or(|expires_at| expires_at <= Utc::now()) {
            return Ok(SubmitProofOutcome::Expired(job));
        }

        Ok(SubmitProofOutcome::Unknown(job))
    }

    /// Terminally fail expired worker jobs that have exhausted their claim attempts.
    ///
    /// Worker execution failure is represented by lock expiry. Jobs remain
    /// reclaimable while `attempt < max_attempts`; once an expired claim reaches
    /// the budget, this reaper transition marks both the worker job and requester
    /// proof `FAILED` and stores `error_message`. The update is batched and uses
    /// `FOR UPDATE SKIP LOCKED` to avoid locking the full expired backlog.
    pub async fn fail_expired_proof_jobs(
        &self,
        req: FailExpiredProofJobs<'_>,
    ) -> Result<Vec<ProofJob>> {
        let columns = PROOF_JOB_RETURNING_COLUMNS;
        let sql = format!(
            r#"
            UPDATE proof_requests
            SET job_status = 'FAILED',
                status = 'FAILED',
                error_message = $2,
                completed_at = NOW()
            WHERE id IN (
                SELECT id
                FROM proof_requests
                WHERE job_status = 'CLAIMED'
                  AND lock_expires_at < NOW()
                  AND attempt >= $1
                ORDER BY lock_expires_at ASC, start_block_number ASC, created_at ASC, id ASC
                LIMIT $3
                FOR UPDATE SKIP LOCKED
            )
            RETURNING {columns}
            "#
        );

        let rows = sqlx::query(&sql)
            .bind(i64::from(req.max_attempts))
            .bind(req.error_message)
            .bind(i64::from(req.batch_size))
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(row_to_proof_job).collect()
    }

    /// Retry a stuck PENDING request if under the retry limit, otherwise fail it permanently.
    ///
    /// If `retry_count < max_retries`: atomically resets to CREATED, increments `retry_count`,
    /// and resets the worker job lifecycle so the request can be claimed again.
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
                job_status = 'PENDING',
                retry_count = retry_count + 1,
                error_message = NULL,
                stark_receipt = NULL,
                snark_receipt = NULL,
                result_payload = NULL,
                submitted_by_worker_id = NULL,
                submitted_lock_id = NULL,
                completed_at = NULL,
                worker_id = NULL,
                lock_id = NULL,
                lock_expires_at = NULL,
                claimed_at = NULL,
                last_heartbeat_at = NULL,
                attempt = 0
            WHERE id = $2
            "#,
        )
        .bind(ProofStatus::Created.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;

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
            SELECT id, COALESCE(session_id, id::text) AS session_id,
                   request_payload, api_proof_type, zk_vm, tee_kind,
                   start_block_number, number_of_blocks_to_prove,
                   sequence_window, proof_type, stark_receipt, snark_receipt,
                   result_payload, submitted_by_worker_id, submitted_lock_id,
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
                pr.id, COALESCE(pr.session_id, pr.id::text) AS session_id,
                pr.request_payload, pr.api_proof_type, pr.zk_vm,
                pr.tee_kind, pr.start_block_number, pr.number_of_blocks_to_prove,
                pr.sequence_window, pr.proof_type, pr.stark_receipt, pr.snark_receipt,
                pr.result_payload, pr.submitted_by_worker_id, pr.submitted_lock_id,
                pr.status, pr.error_message, pr.prover_address, pr.l1_head,
                pr.intermediate_root_interval,
                pr.created_at, pr.updated_at, pr.completed_at, pr.retry_count
            FROM proof_requests pr
            WHERE pr.status = 'PENDING'
              AND pr.proof_type IS NOT NULL
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
        let result_payload = result_payload_from_receipt_update(&update_receipt)?;

        let result = sqlx::query(
            r#"
            UPDATE proof_requests
            SET
                stark_receipt = COALESCE($1, stark_receipt),
                snark_receipt = COALESCE($2, snark_receipt),
                result_payload = COALESCE($3, result_payload),
                status = $4,
                error_message = $5,
                completed_at = CASE WHEN $4 IN ('SUCCEEDED', 'FAILED') THEN NOW() ELSE completed_at END
            WHERE id = $6
              AND status = 'RUNNING'
            "#,
        )
        .bind(&update_receipt.stark_receipt)
        .bind(&update_receipt.snark_receipt)
        .bind(&result_payload)
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
                    id, COALESCE(session_id, id::text) AS session_id,
                    request_payload, api_proof_type, zk_vm, tee_kind,
                    start_block_number, number_of_blocks_to_prove, sequence_window, proof_type,
                    stark_receipt, snark_receipt, result_payload,
                    submitted_by_worker_id, submitted_lock_id,
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
                    id, COALESCE(session_id, id::text) AS session_id,
                    request_payload, api_proof_type, zk_vm, tee_kind,
                    start_block_number, number_of_blocks_to_prove, sequence_window, proof_type,
                    stark_receipt, snark_receipt, result_payload,
                    submitted_by_worker_id, submitted_lock_id,
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
    ///
    /// An empty status filter returns all proof requests.
    pub async fn list_with_offset(
        &self,
        status_filter: &[ProofStatus],
        page: ProofRequestPage,
    ) -> Result<(Vec<ProofRequestListItem>, u64)> {
        let (rows, count) = if status_filter.is_empty() {
            let rows = sqlx::query_as::<_, ProofRequestListItem>(
                r#"
                SELECT
                    id,
                    COALESCE(session_id, id::text) AS session_id,
                    COALESCE(
                        api_proof_type,
                        CASE proof_type
                            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
                            ELSE 'compressed'
                        END
                    ) AS api_proof_type,
                    CASE
                        WHEN zk_vm IS NOT NULL THEN zk_vm
                        WHEN COALESCE(
                            api_proof_type,
                            CASE proof_type
                                WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
                                ELSE 'compressed'
                            END
                        ) IN ('compressed', 'snark_groth16') THEN 'sp1'
                        ELSE NULL
                    END AS zk_vm,
                    tee_kind,
                    start_block_number, number_of_blocks_to_prove, proof_type,
                    status, error_message,
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
        } else {
            let statuses = status_filter.iter().map(ProofStatus::as_str).collect::<Vec<_>>();
            let rows = sqlx::query_as::<_, ProofRequestListItem>(
                r#"
                SELECT
                    id,
                    COALESCE(session_id, id::text) AS session_id,
                    COALESCE(
                        api_proof_type,
                        CASE proof_type
                            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
                            ELSE 'compressed'
                        END
                    ) AS api_proof_type,
                    CASE
                        WHEN zk_vm IS NOT NULL THEN zk_vm
                        WHEN COALESCE(
                            api_proof_type,
                            CASE proof_type
                                WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
                                ELSE 'compressed'
                            END
                        ) IN ('compressed', 'snark_groth16') THEN 'sp1'
                        ELSE NULL
                    END AS zk_vm,
                    tee_kind,
                    start_block_number, number_of_blocks_to_prove, proof_type,
                    status, error_message,
                    created_at, updated_at, completed_at
                FROM proof_requests
                WHERE status::text = ANY($1::text[])
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(&statuses)
            .bind(page.limit())
            .bind(page.offset())
            .fetch_all(&self.pool);

            let count = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM proof_requests WHERE status::text = ANY($1::text[])",
            )
            .bind(&statuses)
            .fetch_one(&self.pool);

            futures::try_join!(rows, count)?
        };

        Ok((rows, count.0.max(0) as u64))
    }
}

#[derive(Debug, Clone)]
struct PreparedProofRequest {
    id: Uuid,
    session_id: String,
    request_payload: serde_json::Value,
    api_proof_type: ApiProofType,
    zk_vm: Option<ZkVmKind>,
    tee_kind: Option<TeeKind>,
    start_block_number: i64,
    number_of_blocks_to_prove: i64,
    sequence_window: Option<i64>,
    proof_type: Option<ProofType>,
    prover_address: Option<String>,
    l1_head: Option<String>,
    intermediate_root_interval: Option<i64>,
}

impl TryFrom<CreateProofRequest> for PreparedProofRequest {
    type Error = CreateProofRequestValidationError;

    fn try_from(mut req: CreateProofRequest) -> std::result::Result<Self, Self::Error> {
        req.validate()?;

        let (id, session_id) = canonical_request_ids(&req.session_id)?;
        req.request_payload.session_id = session_id.clone();

        let start_block_number = i64::try_from(req.start_block_number).map_err(|_| {
            CreateProofRequestValidationError::ValueOutOfRange { field: "start_block_number" }
        })?;
        let number_of_blocks_to_prove =
            i64::try_from(req.number_of_blocks_to_prove).map_err(|_| {
                CreateProofRequestValidationError::ValueOutOfRange {
                    field: "number_of_blocks_to_prove",
                }
            })?;
        let sequence_window = req
            .sequence_window
            .map(|w| {
                i64::try_from(w).map_err(|_| CreateProofRequestValidationError::ValueOutOfRange {
                    field: "sequence_window",
                })
            })
            .transpose()?;
        let intermediate_root_interval = req
            .intermediate_root_interval
            .map(|v| {
                i64::try_from(v).map_err(|_| CreateProofRequestValidationError::ValueOutOfRange {
                    field: "intermediate_root_interval",
                })
            })
            .transpose()?;
        validate_backend_proof_type(req.api_proof_type, req.proof_type)?;
        let request_payload = serde_json::to_value(&req.request_payload)
            .map_err(|_| CreateProofRequestValidationError::RequestPayloadSerialization)?;

        Ok(Self {
            id,
            session_id,
            request_payload,
            api_proof_type: req.api_proof_type,
            zk_vm: req.zk_vm,
            tee_kind: req.tee_kind,
            start_block_number,
            number_of_blocks_to_prove,
            sequence_window,
            proof_type: req.proof_type,
            prover_address: req.prover_address,
            l1_head: req.l1_head,
            intermediate_root_interval,
        })
    }
}

fn canonical_request_ids(
    session_id: &str,
) -> std::result::Result<(Uuid, String), CreateProofRequestValidationError> {
    if session_id.is_empty() {
        return Err(CreateProofRequestValidationError::EmptySessionId);
    }

    Uuid::parse_str(session_id)
        .map_or_else(|_| Ok((Uuid::new_v4(), session_id.to_owned())), |id| Ok((id, id.to_string())))
}

const fn validate_backend_proof_type(
    api_proof_type: ApiProofType,
    proof_type: Option<ProofType>,
) -> std::result::Result<(), CreateProofRequestValidationError> {
    match (api_proof_type, proof_type) {
        (ApiProofType::Compressed, Some(ProofType::OpSuccinctSp1ClusterCompressed))
        | (ApiProofType::SnarkGroth16, Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16))
        | (ApiProofType::Tee, None) => Ok(()),
        (ApiProofType::Compressed | ApiProofType::SnarkGroth16, None) => {
            Err(CreateProofRequestValidationError::MissingBackendProofType { api_proof_type })
        }
        (ApiProofType::Tee, Some(_)) => {
            Err(CreateProofRequestValidationError::UnexpectedBackendProofType { api_proof_type })
        }
        (_, Some(proof_type)) => Err(CreateProofRequestValidationError::BackendProofTypeMismatch {
            api_proof_type,
            proof_type,
        }),
    }
}

fn result_payload_from_receipt_update(update: &UpdateReceipt) -> Result<Option<serde_json::Value>> {
    if update.status != ProofStatus::Succeeded {
        return Ok(None);
    }

    let Some(result) = proof_result_from_receipt_update(update) else {
        return Ok(None);
    };

    serde_json::to_value(result).map(Some).map_err(|e| sqlx::Error::Encode(Box::new(e)))
}

fn proof_result_from_receipt_update(update: &UpdateReceipt) -> Option<ProtocolProofResult> {
    // `UpdateReceipt` is the legacy OP Succinct receipt path, which currently only
    // stores SP1 receipts. Protocol-native completions carry their own ZK VM.
    if let Some(snark_receipt) = &update.snark_receipt {
        return Some(ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: snark_receipt.clone().into() },
        }));
    }

    update.stark_receipt.as_ref().map(|stark_receipt| {
        ProtocolProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: stark_receipt.clone().into(),
        })
    })
}

fn compatibility_receipts_for_result(
    result: &ProtocolProofResult,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    match result {
        ProtocolProofResult::Compressed(proof) => (Some(proof.proof.to_vec()), None),
        ProtocolProofResult::SnarkGroth16(proof) => (None, Some(proof.proof.proof.to_vec())),
        ProtocolProofResult::Tee(_) => (None, None),
    }
}

const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

// Legacy rows predate `api_proof_type`; valid legacy ZK rows always have `proof_type`.
// Treat `None` as compressed only to keep reads tolerant of inconsistent pre-migration data.
const fn api_proof_type_for_backend(proof_type: Option<ProofType>) -> ApiProofType {
    match proof_type {
        Some(ProofType::OpSuccinctSp1ClusterCompressed) | None => ApiProofType::Compressed,
        Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16) => ApiProofType::SnarkGroth16,
    }
}

const fn fallback_zk_vm_for_request(api_proof_type: ApiProofType) -> Option<ZkVmKind> {
    match api_proof_type {
        ApiProofType::Compressed | ApiProofType::SnarkGroth16 => Some(ZkVmKind::Sp1),
        ApiProofType::Tee => None,
    }
}

/// Fields needed to build the canonical protocol request payload.
#[derive(Debug, Clone, Copy)]
struct ProtocolRequestPayloadParams<'a> {
    session_id: &'a str,
    start_block_number: i64,
    number_of_blocks_to_prove: i64,
    sequence_window: Option<i64>,
    api_proof_type: ApiProofType,
    tee_kind: Option<TeeKind>,
    prover_address: Option<&'a str>,
    l1_head: Option<&'a str>,
    intermediate_root_interval: Option<i64>,
}

impl ProtocolRequestPayloadParams<'_> {
    fn build(self) -> serde_json::Value {
        let mut zk_payload = serde_json::json!({
            "start_block_number": self.start_block_number,
            "number_of_blocks_to_prove": self.number_of_blocks_to_prove,
            "sequence_window": self.sequence_window,
            "l1_head": self.l1_head,
            "intermediate_root_interval": self.intermediate_root_interval,
            "zk_vm": ZkVmKind::Sp1.as_str(),
        });
        strip_null_object_fields(&mut zk_payload);

        match self.api_proof_type {
            ApiProofType::Compressed => serde_json::json!({
                "session_id": self.session_id,
                "request": {
                    "proof_type": ApiProofType::Compressed.as_str(),
                    "payload": zk_payload,
                },
            }),
            ApiProofType::SnarkGroth16 => serde_json::json!({
                "session_id": self.session_id,
                "request": {
                    "proof_type": ApiProofType::SnarkGroth16.as_str(),
                    "payload": {
                        "proof": zk_payload,
                        "prover_address": self.prover_address.unwrap_or(ZERO_ADDRESS),
                    },
                },
            }),
            ApiProofType::Tee => serde_json::json!({
                "session_id": self.session_id,
                "request": {
                    "proof_type": ApiProofType::Tee.as_str(),
                    "payload": {
                        "proof": {
                            "l1_head": self.l1_head.unwrap_or(ZERO_HASH),
                            "agreed_l2_head_hash": ZERO_HASH,
                            "agreed_l2_output_root": ZERO_HASH,
                            "claimed_l2_output_root": ZERO_HASH,
                            "claimed_l2_block_number": self.start_block_number,
                            "proposer": ZERO_ADDRESS,
                            "intermediate_block_interval": self
                                .intermediate_root_interval
                                .unwrap_or_default(),
                            "l1_head_number": 0,
                            "image_hash": ZERO_HASH,
                        },
                        "tee_kind": self.tee_kind.unwrap_or(TeeKind::AwsNitro).as_str(),
                    },
                },
            }),
        }
    }
}

fn strip_null_object_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, value| {
                strip_null_object_fields(value);
                !value.is_null()
            });
        }
        serde_json::Value::Array(values) => {
            for value in values {
                strip_null_object_fields(value);
            }
        }
        _ => {}
    }
}

fn ensure_protocol_session_id(value: &mut serde_json::Value, session_id: &str) {
    if let serde_json::Value::Object(map) = value
        && map.get("session_id").is_none_or(|value| value.is_null())
    {
        map.insert("session_id".to_owned(), serde_json::Value::String(session_id.to_owned()));
    }
}

/// Helper function to convert a database row to `ProofRequest`
fn row_to_proof_request(row: &sqlx::postgres::PgRow) -> Result<ProofRequest> {
    let id = row.get("id");
    let session_id = row.get::<String, _>("session_id");
    let start_block_number = row.get("start_block_number");
    let number_of_blocks_to_prove = row.get("number_of_blocks_to_prove");
    let sequence_window = row.get("sequence_window");
    let prover_address = row.get::<Option<String>, _>("prover_address");
    let l1_head = row.get::<Option<String>, _>("l1_head");
    let intermediate_root_interval = row.get("intermediate_root_interval");

    let status_str: &str = row.get("status");
    let status = ProofStatus::try_from(status_str)
        .map_err(|e| sqlx::Error::Protocol(format!("Unknown proof status '{status_str}': {e}")))?;

    let proof_type = row
        .get::<Option<&str>, _>("proof_type")
        .map(|proof_type_str| {
            ProofType::try_from(proof_type_str).map_err(|e| {
                sqlx::Error::Protocol(format!("Unknown proof_type '{proof_type_str}': {e}"))
            })
        })
        .transpose()?;

    let api_proof_type = match row.get::<Option<&str>, _>("api_proof_type") {
        Some(value) => ApiProofType::try_from(value)
            .map_err(|e| sqlx::Error::Protocol(format!("Unknown api_proof_type '{value}': {e}")))?,
        None => api_proof_type_for_backend(proof_type),
    };

    let zk_vm = row
        .get::<Option<&str>, _>("zk_vm")
        .map(parse_zk_vm_kind)
        .transpose()?
        .or_else(|| fallback_zk_vm_for_request(api_proof_type));
    let tee_kind = row.get::<Option<&str>, _>("tee_kind").map(parse_tee_kind).transpose()?;
    let mut request_payload =
        row.get::<Option<serde_json::Value>, _>("request_payload").unwrap_or_else(|| {
            ProtocolRequestPayloadParams {
                session_id: &session_id,
                start_block_number,
                number_of_blocks_to_prove,
                sequence_window,
                api_proof_type,
                tee_kind,
                prover_address: prover_address.as_deref(),
                l1_head: l1_head.as_deref(),
                intermediate_root_interval,
            }
            .build()
        });
    ensure_protocol_session_id(&mut request_payload, &session_id);

    Ok(ProofRequest {
        id,
        session_id,
        request_payload,
        api_proof_type,
        zk_vm,
        tee_kind,
        start_block_number,
        number_of_blocks_to_prove,
        sequence_window,
        proof_type,
        stark_receipt: row.get("stark_receipt"),
        snark_receipt: row.get("snark_receipt"),
        result_payload: row.get("result_payload"),
        submitted_by_worker_id: row.get("submitted_by_worker_id"),
        submitted_lock_id: row.get("submitted_lock_id"),
        status,
        error_message: row.get("error_message"),
        prover_address,
        l1_head,
        intermediate_root_interval,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        completed_at: row.get("completed_at"),
        retry_count: row.get("retry_count"),
    })
}

fn parse_zk_vm_kind(value: &str) -> Result<ZkVmKind> {
    ZkVmKind::try_from(value)
        .map_err(|e| sqlx::Error::Protocol(format!("Unknown zk_vm '{value}': {e}")))
}

fn parse_tee_kind(value: &str) -> Result<TeeKind> {
    TeeKind::try_from(value)
        .map_err(|e| sqlx::Error::Protocol(format!("Unknown tee_kind '{value}': {e}")))
}

/// Columns returned by the claim query.
const PROOF_JOB_RETURNING_COLUMNS: &str = "id, COALESCE(session_id, id::text) AS session_id, \
     request_payload, api_proof_type, zk_vm, tee_kind, \
     start_block_number, number_of_blocks_to_prove, sequence_window, proof_type, \
     stark_receipt, snark_receipt, result_payload, \
     submitted_by_worker_id, submitted_lock_id, status, error_message, \
     prover_address, l1_head, intermediate_root_interval, \
     created_at, updated_at, completed_at, retry_count, \
     job_status, worker_id, lock_id, lock_expires_at, claimed_at, attempt, last_heartbeat_at";

/// Capability values bound (as `$5`) into the claim query.
///
/// TEE workers contribute their `tee_kinds`, ZK workers their `zk_vms`. An empty
/// list binds `ANY('{}')`, which matches no rows, so a worker that advertises no
/// matching capabilities simply claims nothing.
fn worker_capability_values(req: &ClaimProofJob) -> Vec<String> {
    match req.api_proof_type {
        ApiProofType::Tee => req.tee_kinds.iter().map(|kind| kind.as_str().to_owned()).collect(),
        ApiProofType::Compressed | ApiProofType::SnarkGroth16 => {
            req.zk_vms.iter().map(|vm| vm.as_str().to_owned()).collect()
        }
    }
}

/// Build the atomic claim query for a proof type.
///
/// The capability column (`tee_kind` for TEE, `zk_vm` for ZK) is hardcoded as a
/// literal in each variant rather than interpolated from a value, so no
/// caller-derived string can ever reach the SQL as a column name. The only
/// interpolated token is the fixed [`PROOF_JOB_RETURNING_COLUMNS`] constant.
fn claim_query(api_proof_type: ApiProofType) -> String {
    let columns = PROOF_JOB_RETURNING_COLUMNS;
    match api_proof_type {
        ApiProofType::Tee => format!(
            r#"
            UPDATE proof_requests
            SET job_status = 'CLAIMED',
                status = 'RUNNING',
                worker_id = $1,
                lock_id = $2,
                attempt = attempt + 1,
                claimed_at = NOW(),
                last_heartbeat_at = NOW(),
                lock_expires_at = NOW() + ($3)::double precision * INTERVAL '1 second'
            WHERE id = (
                SELECT id FROM proof_requests
                WHERE api_proof_type = $4
                  AND tee_kind = ANY($5::text[])
                  AND (
                      job_status = 'PENDING'
                      OR (job_status = 'CLAIMED' AND lock_expires_at < NOW() AND attempt < $6)
                  )
                ORDER BY start_block_number ASC, created_at ASC, id ASC
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING {columns}
            "#,
        ),
        ApiProofType::Compressed | ApiProofType::SnarkGroth16 => format!(
            r#"
            UPDATE proof_requests
            SET job_status = 'CLAIMED',
                status = 'RUNNING',
                worker_id = $1,
                lock_id = $2,
                attempt = attempt + 1,
                claimed_at = NOW(),
                last_heartbeat_at = NOW(),
                lock_expires_at = NOW() + ($3)::double precision * INTERVAL '1 second'
            WHERE id = (
                SELECT id FROM proof_requests
                WHERE api_proof_type = $4
                  AND zk_vm = ANY($5::text[])
                  AND (
                      job_status = 'PENDING'
                      OR (job_status = 'CLAIMED' AND lock_expires_at < NOW() AND attempt < $6)
                  )
                ORDER BY start_block_number ASC, created_at ASC, id ASC
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING {columns}
            "#,
        ),
    }
}

/// Convert a database row into a [`ProofJob`], reusing [`row_to_proof_request`]
/// for the requester fields (including protocol payload synthesis for legacy rows).
fn row_to_proof_job(row: &sqlx::postgres::PgRow) -> Result<ProofJob> {
    let base = row_to_proof_request(row)?;

    let job_status_str: &str = row.get("job_status");
    let job_status = ProofJobStatus::try_from(job_status_str).map_err(|e| {
        sqlx::Error::Protocol(format!("Unknown job_status '{job_status_str}': {e}"))
    })?;

    Ok(ProofJob {
        id: base.id,
        session_id: base.session_id,
        request_payload: base.request_payload,
        api_proof_type: base.api_proof_type,
        zk_vm: base.zk_vm,
        tee_kind: base.tee_kind,
        job_status,
        attempt: row.get("attempt"),
        worker_id: row.get("worker_id"),
        lock_id: row.get("lock_id"),
        lock_expires_at: row.get("lock_expires_at"),
        claimed_at: row.get("claimed_at"),
        last_heartbeat_at: row.get("last_heartbeat_at"),
        error_message: base.error_message,
        created_at: base.created_at,
        updated_at: base.updated_at,
        completed_at: base.completed_at,
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
struct CreateRequestParams<'a> {
    request_payload: &'a serde_json::Value,
    api_proof_type: &'a str,
    zk_vm: Option<&'a str>,
    tee_kind: Option<&'a str>,
    start_block_number: i64,
    number_of_blocks_to_prove: i64,
    sequence_window: Option<i64>,
    proof_type: Option<&'a str>,
    prover_address: Option<&'a str>,
    l1_head: Option<&'a str>,
    intermediate_root_interval: Option<i64>,
}

impl CreateRequestParams<'_> {
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
        if row.get::<Option<&str>, _>("proof_type") != self.proof_type {
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
        if let Some(api_proof_type) = row.get::<Option<&str>, _>("api_proof_type")
            && api_proof_type != self.api_proof_type
        {
            return Some("api_proof_type");
        }
        if let Some(zk_vm) = row.get::<Option<&str>, _>("zk_vm")
            && Some(zk_vm) != self.zk_vm
        {
            return Some("zk_vm");
        }
        if let Some(tee_kind) = row.get::<Option<&str>, _>("tee_kind")
            && Some(tee_kind) != self.tee_kind
        {
            return Some("tee_kind");
        }
        if let Some(request_payload) = row.get::<Option<serde_json::Value>, _>("request_payload")
            && request_payload != *self.request_payload
        {
            return Some("request_payload");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{
        ProofRequest as ProtocolProofRequest, ProofRequestKind, TeeKind as ProtocolTeeKind,
        TeeProofRequest, ZkProofRequest, ZkVm,
    };

    use super::*;

    #[test]
    fn prepared_request_uses_uuid_session_id_and_builds_protocol_payload() {
        let session_id = Uuid::new_v4();
        let create = CreateProofRequest::new(ProtocolProofRequest {
            session_id: session_id.to_string(),
            request: ProofRequestKind::Compressed(ZkProofRequest {
                start_block_number: 100,
                number_of_blocks_to_prove: 5,
                sequence_window: Some(50),
                l1_head: None,
                intermediate_root_interval: Some(5),
                zk_vm: ZkVm::Sp1,
            }),
        })
        .expect("request should validate");
        let prepared = PreparedProofRequest::try_from(create).expect("request should prepare");

        assert_eq!(prepared.id, session_id);
        assert_eq!(prepared.api_proof_type, ApiProofType::Compressed);
        assert_eq!(prepared.zk_vm, Some(ZkVmKind::Sp1));
        assert!(prepared.tee_kind.is_none());

        let protocol_request: ProtocolProofRequest =
            serde_json::from_value(prepared.request_payload).expect("payload should deserialize");
        let session_id = session_id.to_string();
        assert_eq!(protocol_request.session_id, session_id);
        let ProofRequestKind::Compressed(zk_request) = protocol_request.request else {
            panic!("expected compressed protocol request");
        };
        assert_eq!(zk_request.start_block_number, 100);
        assert_eq!(zk_request.number_of_blocks_to_prove, 5);
        assert_eq!(zk_request.sequence_window, Some(50));
        assert_eq!(zk_request.intermediate_root_interval, Some(5));
        assert_eq!(zk_request.zk_vm, ZkVm::Sp1);
    }

    #[test]
    fn prepared_request_omits_absent_optional_protocol_payload_fields() {
        let create = CreateProofRequest::new(ProtocolProofRequest {
            session_id: Uuid::new_v4().to_string(),
            request: ProofRequestKind::Compressed(ZkProofRequest {
                start_block_number: 100,
                number_of_blocks_to_prove: 5,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            }),
        })
        .expect("request should validate");
        let prepared = PreparedProofRequest::try_from(create).expect("request should prepare");

        let payload = prepared
            .request_payload
            .pointer("/request/payload")
            .and_then(serde_json::Value::as_object)
            .expect("compressed payload should be an object");

        assert!(!payload.contains_key("sequence_window"));
        assert!(!payload.contains_key("l1_head"));
        assert!(!payload.contains_key("intermediate_root_interval"));
    }

    #[test]
    fn tee_request_does_not_fallback_to_zk_vm() {
        let zk_vm = fallback_zk_vm_for_request(ApiProofType::Tee);

        assert!(zk_vm.is_none());
    }

    #[test]
    fn fallback_tee_protocol_payload_deserializes() {
        let payload = ProtocolRequestPayloadParams {
            session_id: "tee-session",
            start_block_number: 100,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            api_proof_type: ApiProofType::Tee,
            tee_kind: Some(TeeKind::AwsNitro),
            prover_address: None,
            l1_head: Some(ZERO_HASH),
            intermediate_root_interval: Some(10),
        }
        .build();

        let protocol_request: ProtocolProofRequest =
            serde_json::from_value(payload).expect("fallback TEE payload should deserialize");

        assert_eq!(protocol_request.session_id, "tee-session");
        let ProofRequestKind::Tee(request) = protocol_request.request else {
            panic!("expected TEE protocol request");
        };
        assert_eq!(request.tee_kind, ProtocolTeeKind::AwsNitro);
        assert_eq!(request.proof.claimed_l2_block_number, 100);
        assert_eq!(request.proof.intermediate_block_interval, 10);
    }

    #[test]
    fn prepared_request_represents_tee_protocol_request() {
        let create = CreateProofRequest::new(ProtocolProofRequest {
            session_id: "tee-session".to_owned(),
            request: ProofRequestKind::Tee(TeeProofRequest {
                proof: Default::default(),
                tee_kind: ProtocolTeeKind::AwsNitro,
            }),
        })
        .expect("TEE request should validate");

        let prepared = PreparedProofRequest::try_from(create).expect("TEE request should prepare");

        assert_eq!(prepared.session_id, "tee-session");
        assert_eq!(prepared.api_proof_type, ApiProofType::Tee);
        assert!(prepared.zk_vm.is_none());
        assert_eq!(prepared.tee_kind, Some(TeeKind::AwsNitro));
        assert!(prepared.proof_type.is_none());

        let protocol_request: ProtocolProofRequest =
            serde_json::from_value(prepared.request_payload).expect("payload should deserialize");
        assert_eq!(protocol_request.session_id, "tee-session");
        assert!(matches!(protocol_request.request, ProofRequestKind::Tee(_)));
    }

    #[test]
    fn prepared_request_rejects_unsupported_protocol_combination() {
        let mut create = CreateProofRequest::new(ProtocolProofRequest {
            session_id: "bad-tee-session".to_owned(),
            request: ProofRequestKind::Tee(TeeProofRequest {
                proof: Default::default(),
                tee_kind: ProtocolTeeKind::AwsNitro,
            }),
        })
        .expect("TEE request should validate");
        create.zk_vm = Some(ZkVmKind::Sp1);

        let err =
            PreparedProofRequest::try_from(create).expect_err("invalid TEE request should fail");

        assert_eq!(err, CreateProofRequestValidationError::FieldMismatch { field: "zk_vm" });
    }

    #[test]
    fn prepared_request_rejects_database_range_overflow() {
        let create = CreateProofRequest::new(ProtocolProofRequest {
            session_id: Uuid::new_v4().to_string(),
            request: ProofRequestKind::Compressed(ZkProofRequest {
                start_block_number: (i64::MAX as u64) + 1,
                number_of_blocks_to_prove: 5,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            }),
        })
        .expect("request should validate");

        let err = PreparedProofRequest::try_from(create)
            .expect_err("out-of-range request should fail preparation");

        assert_eq!(
            err,
            CreateProofRequestValidationError::ValueOutOfRange { field: "start_block_number" }
        );
    }

    #[test]
    fn receipt_update_builds_compressed_result_payload() {
        let update = UpdateReceipt {
            id: Uuid::new_v4(),
            stark_receipt: Some(vec![1, 2, 3]),
            snark_receipt: None,
            status: ProofStatus::Succeeded,
            error_message: None,
        };

        let payload = result_payload_from_receipt_update(&update)
            .expect("payload should serialize")
            .expect("stark receipt should produce payload");
        let result: ProtocolProofResult =
            serde_json::from_value(payload).expect("payload should deserialize");

        assert_eq!(
            result,
            ProtocolProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![1, 2, 3].into()
            })
        );
    }

    #[test]
    fn receipt_update_skips_non_terminal_result_payload() {
        let update = UpdateReceipt {
            id: Uuid::new_v4(),
            stark_receipt: Some(vec![1, 2, 3]),
            snark_receipt: None,
            status: ProofStatus::Running,
            error_message: None,
        };

        assert!(
            result_payload_from_receipt_update(&update)
                .expect("payload check should not fail")
                .is_none()
        );
    }

    #[test]
    fn receipt_update_prefers_snark_result_payload() {
        let update = UpdateReceipt {
            id: Uuid::new_v4(),
            stark_receipt: Some(vec![1, 2, 3]),
            snark_receipt: Some(vec![4, 5, 6]),
            status: ProofStatus::Succeeded,
            error_message: None,
        };

        let payload = result_payload_from_receipt_update(&update)
            .expect("payload should serialize")
            .expect("snark receipt should produce payload");
        let result: ProtocolProofResult =
            serde_json::from_value(payload).expect("payload should deserialize");

        assert_eq!(
            result,
            ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![4, 5, 6].into() }
            })
        );
    }
}
