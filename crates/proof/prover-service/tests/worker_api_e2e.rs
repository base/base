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

use base_proof_primitives::Proposal;
use base_prover_service::{ProverServiceServer, ServerConfig, WorkerApiConfig, WorkerQueueConfig};
use base_prover_service_db::{
    ApiProofType, ClaimProofJob, CreateProofRequest, DatabaseConfig, ProofRequestRepo,
    ProofStatus as DbProofStatus, TeeKind as DbTeeKind, ZkVmKind,
};
use base_prover_service_protocol::{
    AbandonProofRequest, BackendSessionState, DeleteProofsByUnregisteredTeeSignersRequest,
    GetNextProofRequest, GetProofRequest, GetProofSessionRequest, HeartbeatRequest, ProofJobStatus,
    ProofRequest as ProtocolProofRequest, ProofRequestKind, ProofResult, ProofType,
    ProverRequesterApiClient, ProverRequesterApiServer, ProverWorkerApiClient,
    ProverWorkerApiServer, RecordProofSessionRequest, SessionType, TeeKind as ProtocolTeeKind,
    TeeProofRequest, TeeProofResult, WorkerSubmitProofRequest, ZkBackend, ZkProofRequest,
    ZkProofResult, ZkVm,
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
            schedule_l2_block_number: None,
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
async fn requester_bulk_signer_cleanup_round_trip() {
    let repo = test_repo().await;
    let drain = ClaimProofJob {
        worker_id: "bulk-rpc-drain".to_owned(),
        api_proof_type: ApiProofType::Tee,
        tee_kinds: vec![DbTeeKind::AwsNitro],
        zk_vms: Vec::new(),
        zk_backends: Vec::new(),
        lock_duration_seconds: 3600,
        max_attempts: u32::MAX,
    };
    while repo.claim_next_proof_job(drain.clone()).await.unwrap().is_some() {}
    let server = RunningServer::spawn(repo.clone()).await;
    let signers = [
        "0x1111111111111111111111111111111111111111",
        "0x2222222222222222222222222222222222222222",
        "0x3333333333333333333333333333333333333333",
        "0x0000000000000000000000000000000000000000",
    ]
    .map(|signer| signer.parse().unwrap());
    let mut sessions = Vec::new();
    for signer in signers {
        let session = Uuid::new_v4().to_string();
        let request = CreateProofRequest::new(ProtocolProofRequest {
            session_id: session.clone(),
            request: ProofRequestKind::Tee(TeeProofRequest {
                proof: Default::default(),
                tee_kind: ProtocolTeeKind::AwsNitro,
            }),
        })
        .unwrap();
        repo.create_for_worker_queue(request, TEST_MAX_PROOF_RETRIES, true).await.unwrap();
        let claimed = server
            .client
            .get_next_proof(GetNextProofRequest {
                worker_id: "bulk-rpc-worker".to_owned(),
                proof_type: ProofType::Tee,
                tee_kinds: vec![ProtocolTeeKind::AwsNitro],
                zk_vms: Vec::new(),
                zk_backends: Vec::new(),
                lock_duration_seconds: 60,
            })
            .await
            .unwrap()
            .job
            .unwrap();
        assert_eq!(claimed.session_id, session);
        server
            .client
            .submit_proof(WorkerSubmitProofRequest {
                session_id: session.clone(),
                lock_id: claimed.lock_id.unwrap(),
                worker_id: "bulk-rpc-worker".to_owned(),
                result: ProofResult::Tee(TeeProofResult {
                    aggregate_proposal: Proposal {
                        output_root: Default::default(),
                        signature: Default::default(),
                        l1_origin_hash: Default::default(),
                        l1_origin_number: 0,
                        l2_block_number: 0,
                        prev_output_root: Default::default(),
                        config_hash: Default::default(),
                        schedule_id: Default::default(),
                    },
                    proposals: Vec::new(),
                    tee_kind: ProtocolTeeKind::AwsNitro,
                    tee_signer: signer,
                }),
            })
            .await
            .unwrap();
        sessions.push(session);
    }

    let deleted = server
        .client
        .delete_proofs_by_unregistered_tee_signers(DeleteProofsByUnregisteredTeeSignersRequest {
            registered_signers: vec![signers[2]],
        })
        .await
        .unwrap();
    assert!(deleted >= 2);
    for session in &sessions[..2] {
        let error = server
            .client
            .get_proof(GetProofRequest { session_id: session.clone() })
            .await
            .unwrap_err();
        assert_rpc_error_code(&error, ERROR_NOT_FOUND);
    }
    for session in &sessions[2..] {
        assert!(repo.get_by_session_id(session).await.unwrap().is_some());
    }
    let error = server
        .client
        .delete_proofs_by_unregistered_tee_signers(DeleteProofsByUnregisteredTeeSignersRequest {
            registered_signers: vec![Default::default()],
        })
        .await
        .unwrap_err();
    assert_rpc_error_code(&error, jsonrpsee::types::ErrorCode::InvalidParams.code());
    assert!(repo.get_by_session_id(&sessions[2]).await.unwrap().is_some());
    assert_eq!(
        server
            .client
            .delete_proofs_by_unregistered_tee_signers(
                DeleteProofsByUnregisteredTeeSignersRequest { registered_signers: Vec::new() },
            )
            .await
            .unwrap(),
        1
    );
    assert!(repo.get_by_session_id(&sessions[2]).await.unwrap().is_none());
    assert!(repo.get_by_session_id(&sessions[3]).await.unwrap().is_some());
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL)"]
async fn worker_claim_heartbeat_submit_round_trip() {
    let repo = test_repo().await;
    drain_claimable_compressed_jobs(&repo).await;

    let session_id = Uuid::new_v4().to_string();
    repo.create_for_worker_queue(compressed_request(&session_id, 10), TEST_MAX_PROOF_RETRIES, true)
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

    server
        .client
        .record_proof_session(RecordProofSessionRequest {
            session_id: session_id.clone(),
            lock_id: lock_id.clone(),
            worker_id: "worker-e2e".to_owned(),
            session_type: SessionType::Stark,
            backend_session_id: "backend-e2e".to_owned(),
            state: BackendSessionState::Running,
        })
        .await
        .expect("recording an active backend session should succeed");

    let stale_abandon = server
        .client
        .abandon_proof(AbandonProofRequest {
            session_id: session_id.clone(),
            lock_id: Uuid::new_v4().to_string(),
            worker_id: "worker-e2e".to_owned(),
            error_message: "stale worker failure".to_owned(),
        })
        .await
        .expect_err("a stale fencing token must not abandon the job");
    assert_rpc_error_code(&stale_abandon, ERROR_FAILED_PRECONDITION);

    let abandoned = server
        .client
        .abandon_proof(AbandonProofRequest {
            session_id: session_id.clone(),
            lock_id: lock_id.clone(),
            worker_id: "worker-e2e".to_owned(),
            error_message: "generation failed".to_owned(),
        })
        .await
        .expect("the owning worker should abandon the job");
    assert_eq!(abandoned.job.status, ProofJobStatus::Pending);
    assert_eq!(abandoned.job.attempt, 1);

    let active_session = server
        .client
        .get_proof_session(GetProofSessionRequest {
            session_id: session_id.clone(),
            session_type: SessionType::Stark,
        })
        .await
        .expect("backend session lookup should succeed")
        .session
        .expect("active backend session should survive requeue");
    assert_eq!(active_session.backend_session_id, "backend-e2e");
    assert_eq!(active_session.state, BackendSessionState::Running);

    let reclaimed = server
        .client
        .get_next_proof(worker_claim("worker-e2e-reclaim"))
        .await
        .expect("abandoned job should be immediately reclaimable")
        .job
        .expect("abandoned job should be returned without waiting for lease expiry");
    assert_eq!(reclaimed.session_id, session_id);
    assert_eq!(reclaimed.attempt, 2);
    let reclaimed_lock_id = reclaimed.lock_id.expect("reclaimed job carries a new lock id");
    assert_ne!(reclaimed_lock_id, lock_id);

    let submitted = server
        .client
        .submit_proof(WorkerSubmitProofRequest {
            session_id: session_id.clone(),
            lock_id: reclaimed_lock_id,
            worker_id: "worker-e2e-reclaim".to_owned(),
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
    assert_eq!(stored.submitted_by_worker_id.as_deref(), Some("worker-e2e-reclaim"));

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
