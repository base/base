//! Postgres-backed round-trip tests for the prover worker JSON-RPC API.
//!
//! These boot an in-process JSON-RPC server backed by a real [`ProofRequestRepo`]
//! and exercise the typed worker client against it.
//!
//! Run with:
//! ```sh
//! DATABASE_URL=postgres://prover:prover@localhost:5433/prover \
//!   cargo nextest run --run-ignored all -p base-prover-service --test worker_api_e2e --test-threads=1
//! ```
//!
//! Tests are marked `#[ignore]` so they're skipped by default.

use std::{net::SocketAddr, time::Duration};

use base_prover_service::{ProverServiceServer, ServerConfig, WorkerApiConfig, WorkerQueueConfig};
use base_prover_service_db::{
    ApiProofType, ClaimProofJob, CreateProofRequest, DatabaseConfig, ProofRequestRepo,
    ProofStatus as DbProofStatus, ZkVmKind,
};
use base_prover_service_protocol::{
    GetNextProofRequest, HeartbeatRequest, ProofJobStatus, ProofRequest as ProtocolProofRequest,
    ProofRequestKind, ProofResult, ProofType, ProverRequesterApiServer, ProverWorkerApiClient,
    ProverWorkerApiServer, WorkerSubmitProofRequest, ZkBackend, ZkProofRequest, ZkProofResult,
    ZkVm,
};
use jsonrpsee::{
    core::client::Error as ClientError,
    http_client::{HttpClient, HttpClientBuilder},
    server::{Server, ServerHandle},
};
use uuid::Uuid;

/// Proof request retry cap; must match prover `MAX_PROOF_RETRIES` default (`3`).
const TEST_MAX_PROOF_RETRIES: i32 = 3;

/// Mirror of the server-side `FAILED_PRECONDITION` JSON-RPC error code.
const ERROR_FAILED_PRECONDITION: i32 = -32017;
/// Mirror of the server-side `NOT_FOUND` JSON-RPC error code.
const ERROR_NOT_FOUND: i32 = -32004;

/// Assert that a worker RPC error carries the expected non-retryable code.
#[track_caller]
fn assert_rpc_error_code(err: &ClientError, expected_code: i32) {
    match err {
        ClientError::Call(call) => assert_eq!(call.code(), expected_code, "unexpected error code"),
        other => panic!("expected a JSON-RPC call error, got: {other:?}"),
    }
}

async fn test_repo() -> ProofRequestRepo {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://prover:prover@localhost:5433/prover".to_string());
    let pool =
        DatabaseConfig { url, max_connections: 5, connection_timeout: Duration::from_secs(5) }
            .init_pool()
            .await
            .expect("failed to connect to test database; is Postgres running?");

    ProofRequestRepo::new(pool)
}

/// In-process JSON-RPC server serving both requester and worker APIs.
struct RunningServer {
    client: HttpClient,
    handle: ServerHandle,
}

impl RunningServer {
    async fn spawn(repo: ProofRequestRepo) -> Self {
        let config = ServerConfig {
            max_proof_retries: TEST_MAX_PROOF_RETRIES,
            worker: WorkerApiConfig::default(),
            worker_queue: WorkerQueueConfig::default(),
        };
        let server = ProverServiceServer::new(repo, config);

        let mut module = ProverWorkerApiServer::into_rpc(server.clone());
        module
            .merge(ProverRequesterApiServer::into_rpc(server))
            .expect("requester and worker namespaces should not collide");

        let addr: SocketAddr = "127.0.0.1:0".parse().expect("test address should parse");
        let rpc_server = Server::builder().build(addr).await.expect("server should bind");
        let local_addr = rpc_server.local_addr().expect("server should have a local address");
        let handle = rpc_server.start(module);

        let client = HttpClientBuilder::default()
            .build(format!("http://{local_addr}"))
            .expect("client should build");

        Self { client, handle }
    }

    async fn shutdown(self) {
        self.handle.stop().expect("server should stop");
        self.handle.stopped().await;
    }
}

/// Claim every currently-claimable compressed job under a long lease so the test
/// starts from a known-empty compressed queue, independent of leftover rows.
async fn drain_claimable_compressed_jobs(repo: &ProofRequestRepo) {
    let drain = ClaimProofJob {
        worker_id: "worker-api-e2e-drain".to_owned(),
        api_proof_type: ApiProofType::Compressed,
        tee_kinds: Vec::new(),
        zk_vms: vec![ZkVmKind::Sp1],
        zk_backends: vec![ZkBackend::Cluster],
        lock_duration_seconds: 3600,
        max_attempts: u32::MAX,
    };

    while repo
        .claim_next_proof_job(drain.clone())
        .await
        .expect("drain claim should not error")
        .is_some()
    {}
}

fn compressed_request(session_id: &str, start_block_number: u64) -> CreateProofRequest {
    CreateProofRequest::new(ProtocolProofRequest {
        session_id: session_id.to_owned(),
        request: ProofRequestKind::Compressed(ZkProofRequest {
            start_block_number,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
        }),
    })
    .expect("compressed request should validate")
}

fn worker_claim(worker_id: &str) -> GetNextProofRequest {
    GetNextProofRequest {
        worker_id: worker_id.to_owned(),
        proof_type: ProofType::Compressed,
        tee_kinds: Vec::new(),
        zk_vms: vec![ZkVm::Sp1],
        // Omitted by legacy workers; the server defaults this capability to cluster.
        zk_backends: Vec::new(),
        lock_duration_seconds: 60,
    }
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL)"]
async fn worker_claim_heartbeat_submit_round_trip() {
    let repo = test_repo().await;
    drain_claimable_compressed_jobs(&repo).await;

    let session_id = Uuid::new_v4().to_string();
    repo.create_for_worker_queue(compressed_request(&session_id, 10), TEST_MAX_PROOF_RETRIES)
        .await
        .expect("seeding the worker queue should succeed");

    let server = RunningServer::spawn(repo.clone()).await;

    let claimed = server
        .client
        .get_next_proof(worker_claim("worker-e2e"))
        .await
        .expect("get_next_proof should succeed")
        .job
        .expect("a claimable compressed job should be returned");
    assert_eq!(claimed.session_id, session_id);
    assert_eq!(claimed.status, ProofJobStatus::Claimed);
    assert_eq!(claimed.worker_id.as_deref(), Some("worker-e2e"));
    let lock_id = claimed.lock_id.clone().expect("a claimed job carries a lock id");

    let beat = server
        .client
        .heartbeat(HeartbeatRequest {
            session_id: session_id.clone(),
            lock_id: lock_id.clone(),
            worker_id: "worker-e2e".to_owned(),
            lock_duration_seconds: 60,
        })
        .await
        .expect("heartbeat with the owning lock should succeed");
    assert_eq!(beat.job.status, ProofJobStatus::Claimed);

    let stale = server
        .client
        .heartbeat(HeartbeatRequest {
            session_id: session_id.clone(),
            lock_id: Uuid::new_v4().to_string(),
            worker_id: "worker-e2e".to_owned(),
            lock_duration_seconds: 60,
        })
        .await
        .expect_err("a non-owning lock must be rejected");
    assert_rpc_error_code(&stale, ERROR_FAILED_PRECONDITION);

    let submitted = server
        .client
        .submit_proof(WorkerSubmitProofRequest {
            session_id: session_id.clone(),
            lock_id: lock_id.clone(),
            worker_id: "worker-e2e".to_owned(),
            result: ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![1, 2, 3].into(),
                execution_stats: None,
            }),
        })
        .await
        .expect("submit_proof with the owning lock should succeed");
    assert_eq!(submitted.job.status, ProofJobStatus::Succeeded);

    let stored = repo
        .get_by_session_id(&session_id)
        .await
        .expect("get_by_session_id should succeed")
        .expect("the submitted proof request should exist");
    assert_eq!(stored.status, DbProofStatus::Succeeded);
    assert!(stored.result_payload.is_some(), "submitted result payload should be persisted");
    assert_eq!(stored.submitted_by_worker_id.as_deref(), Some("worker-e2e"));

    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL)"]
async fn worker_submit_unknown_session_is_not_found() {
    let repo = test_repo().await;
    let server = RunningServer::spawn(repo).await;

    let err = server
        .client
        .submit_proof(WorkerSubmitProofRequest {
            session_id: Uuid::new_v4().to_string(),
            lock_id: Uuid::new_v4().to_string(),
            worker_id: "worker-missing".to_owned(),
            result: ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![9].into(),
                execution_stats: None,
            }),
        })
        .await
        .expect_err("submitting against an unknown session should fail");
    assert_rpc_error_code(&err, ERROR_NOT_FOUND);

    server.shutdown().await;
}
