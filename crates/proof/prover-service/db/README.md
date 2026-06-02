# `base-prover-service-db`

`PostgreSQL` persistence layer for prover-service proof requests and sessions.

## Worker Job Model

`proof_requests` is the source of truth for requester-submitted proof jobs. The
requester lifecycle is stored in `status` (`CREATED`, `PENDING`, `RUNNING`,
`SUCCEEDED`, `FAILED`), while the external worker lifecycle is stored in
`job_status` (`PENDING`, `CLAIMED`, `SUCCEEDED`, `FAILED`). Backend-specific
`proof_sessions` rows remain an implementation detail for providers that need
asynchronous STARK/SNARK sessions.

Workers claim jobs directly from `proof_requests`. Every active claim has a
`worker_id`, `lock_id`, `lock_expires_at`, and `attempt`; heartbeat and submit
updates are guarded by all ownership fields plus an unexpired lock. Worker
execution failure is represented by lock expiry rather than an explicit
`failProof` API: expired claims are reclaimable while `attempt < max_attempts`,
and `fail_expired_proof_jobs` terminally fails expired jobs in bounded batches
once the retry budget is exhausted.
