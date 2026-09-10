# `base-prover-service-db`

`PostgreSQL` persistence layer for prover-service proof requests and sessions.

## Worker Job Ownership

`proof_requests` is the canonical queue for requester-submitted proof work.
Requester lifecycle is stored in `status`; worker lifecycle is stored in
`job_status`. New protocol-native requests are inserted with
`job_status = 'PENDING'` and are claimed through the worker API with lock
fencing, heartbeats, and guarded proof submission.

External workers claim both ZK and TEE jobs directly from `proof_requests`.
`proof_sessions` remains available for backend-specific ZK session tracking.
