//! Integration tests for [`ProofRequestRepo`] against a real `PostgreSQL` database.
//!
//! These tests require a running Postgres instance with the prover schema applied.
//!
//! Run with:
//!   ```sh
//!   DATABASE_URL=postgres://prover:prover@localhost:5433/prover \
//!     cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1
//!   ```
//!
//! Tests are marked `#[ignore]` so they're skipped by default (no Postgres in CI)
//! and must be opted into explicitly via `--run-ignored all`.
//! They run sequentially (`--test-threads=1`) because they share the same database;
//! each test creates unique UUIDs so they don't collide.

use std::time::Duration;

use base_prover_service_db::{
    ApiProofType, ClaimProofJob, CompleteClaimedProofJob, CreateProofRequest,
    CreateProofRequestOutcome, CreateProofSession, FailExpiredProofJobs, HeartbeatOutcome,
    HeartbeatProofJob, ProofJobStatus, ProofRequestPage, ProofRequestRepo, ProofStatus, ProofType,
    RetryOutcome, SessionStatus, SessionType, SubmitProofOutcome, TeeKind, UpdateProofSession,
    UpdateReceipt, ZkVmKind,
};
use base_prover_service_protocol::{
    ProofRequest as ProtocolProofRequest, ProofRequestKind as ProtocolProofRequestKind,
    ProofResult as ProtocolProofResult, SnarkGroth16ProofRequest, SnarkGroth16ProofResult,
    TeeKind as ProtocolTeeKind, TeeProofRequest, ZkProofRequest, ZkProofResult, ZkVm,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

/// Proof request retry cap; must match prover `MAX_PROOF_RETRIES` default (`3`).
const TEST_MAX_PROOF_RETRIES: i32 = 3;

/// Connect to the test database using `DATABASE_URL` env var.
async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://prover:prover@localhost:5433/prover".to_string());

    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("Failed to connect to test database — is Postgres running?")
}

const fn test_repo(pool: PgPool) -> ProofRequestRepo {
    ProofRequestRepo::new(pool)
}

fn compressed_request() -> CreateProofRequest {
    compressed_request_at(100)
}

fn compressed_request_at(start_block_number: u64) -> CreateProofRequest {
    CreateProofRequest::new(ProtocolProofRequest {
        session_id: Uuid::new_v4().to_string(),
        request: ProtocolProofRequestKind::Compressed(ZkProofRequest {
            start_block_number,
            number_of_blocks_to_prove: 5,
            sequence_window: Some(50),
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        }),
    })
    .expect("compressed request should validate")
}

fn snark_request() -> CreateProofRequest {
    CreateProofRequest::new(ProtocolProofRequest {
        session_id: Uuid::new_v4().to_string(),
        request: ProtocolProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: ZkProofRequest {
                start_block_number: 200,
                number_of_blocks_to_prove: 10,
                sequence_window: Some(100),
                l1_head: Some(
                    "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                        .parse()
                        .expect("valid hash"),
                ),
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            },
            prover_address: "0x1234567890abcdef1234567890abcdef12345678"
                .parse()
                .expect("valid address"),
        }),
    })
    .expect("snark request should validate")
}

fn tee_request() -> CreateProofRequest {
    CreateProofRequest::new(ProtocolProofRequest {
        session_id: Uuid::new_v4().to_string(),
        request: ProtocolProofRequestKind::Tee(TeeProofRequest {
            proof: Default::default(),
            tee_kind: ProtocolTeeKind::AwsNitro,
        }),
    })
    .expect("TEE request should validate")
}

fn set_request_session_id(req: &mut CreateProofRequest, session_id: impl Into<String>) {
    let session_id = session_id.into();
    req.session_id = session_id.clone();
    req.request_payload.session_id = session_id;
}

/// Create a request in RUNNING state with an associated proof session.
/// Returns `(request_id, backend_session_id)`.
async fn setup_running_request(repo: &ProofRequestRepo) -> (Uuid, String) {
    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();
    let backend_id = format!("session-{}", Uuid::new_v4());
    repo.transition_pending_to_running(CreateProofSession {
        proof_request_id: id,
        session_type: SessionType::Stark,
        backend_session_id: backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap()
    .expect("transition should succeed");
    (id, backend_id)
}

// ============================================================
// Basic CRUD tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_and_get_compressed() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    let req = repo.get(id).await.unwrap().expect("should find request");

    assert_eq!(req.id, id);
    assert_eq!(req.session_id, id.to_string());
    assert_eq!(req.api_proof_type, ApiProofType::Compressed);
    assert_eq!(req.zk_vm, Some(ZkVmKind::Sp1));
    assert!(req.tee_kind.is_none());
    serde_json::from_value::<ProtocolProofRequest>(req.request_payload.clone())
        .expect("stored protocol request payload should deserialize");
    assert_eq!(req.start_block_number, 100);
    assert_eq!(req.number_of_blocks_to_prove, 5);
    assert_eq!(req.sequence_window, Some(50));
    assert_eq!(req.proof_type, Some(ProofType::OpSuccinctSp1ClusterCompressed));
    assert_eq!(req.status, ProofStatus::Created);
    assert!(req.stark_receipt.is_none());
    assert!(req.snark_receipt.is_none());
    assert!(req.error_message.is_none());
    assert!(req.prover_address.is_none());
    assert!(req.l1_head.is_none());
    assert!(req.completed_at.is_none());
    assert_eq!(req.retry_count, 0);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_and_get_snark() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(snark_request()).await.unwrap();
    let req = repo.get(id).await.unwrap().expect("should find request");

    assert_eq!(req.start_block_number, 200);
    assert_eq!(req.number_of_blocks_to_prove, 10);
    assert_eq!(req.proof_type, Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16));
    assert_eq!(req.prover_address.as_deref(), Some("0x1234567890abcdef1234567890abcdef12345678"));
    assert_eq!(
        req.l1_head.as_deref(),
        Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
    );
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_with_session_id() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string());

    let id = repo.create(req).await.unwrap();
    assert_eq!(id, explicit_id);

    let fetched = repo.get(id).await.unwrap().expect("should find request");
    assert_eq!(fetched.id, explicit_id);
    assert_eq!(fetched.session_id, explicit_id.to_string());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_with_uppercase_session_id_is_canonicalized() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string().to_uppercase());

    let id = repo.create(req).await.unwrap();
    assert_eq!(id, explicit_id);

    let fetched = repo.get(id).await.unwrap().expect("should find request by UUID session ID");
    assert_eq!(fetched.id, explicit_id);
    assert_eq!(fetched.session_id, explicit_id.to_string());

    let (proofs, _) =
        repo.list_with_offset(&[], ProofRequestPage::try_new(100, 0).unwrap()).await.unwrap();
    assert!(proofs.iter().any(|proof| proof.session_id == fetched.session_id));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_legacy_rollout_request_without_protocol_storage_is_readable_and_replayable() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string());

    sqlx::query(
        r#"
        INSERT INTO proof_requests (
            id, start_block_number, number_of_blocks_to_prove, sequence_window, proof_type, status,
            prover_address, l1_head, intermediate_root_interval
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(explicit_id)
    .bind(i64::try_from(req.start_block_number).unwrap())
    .bind(i64::try_from(req.number_of_blocks_to_prove).unwrap())
    .bind(req.sequence_window.map(|value| i64::try_from(value).unwrap()))
    .bind(req.proof_type.expect("compressed request has proof_type").as_str())
    .bind(ProofStatus::Created.as_str())
    .bind(&req.prover_address)
    .bind(&req.l1_head)
    .bind(req.intermediate_root_interval.map(|value| i64::try_from(value).unwrap()))
    .execute(&pool)
    .await
    .unwrap();

    let fetched = repo.get(explicit_id).await.unwrap().expect("should synthesize protocol fields");
    assert_eq!(fetched.api_proof_type, ApiProofType::Compressed);
    assert_eq!(fetched.zk_vm, Some(ZkVmKind::Sp1));
    serde_json::from_value::<ProtocolProofRequest>(fetched.request_payload)
        .expect("synthesized protocol request payload should deserialize");

    let (proofs, _) = repo
        .list_with_offset(&[ProofStatus::Created], ProofRequestPage::try_new(10_000, 0).unwrap())
        .await
        .unwrap();
    let listed = proofs
        .iter()
        .find(|proof| proof.id == explicit_id)
        .expect("list should synthesize protocol fields");
    assert_eq!(listed.api_proof_type, ApiProofType::Compressed);
    assert_eq!(listed.zk_vm, Some(ZkVmKind::Sp1));

    let outcome = repo.create_for_worker_queue(req, TEST_MAX_PROOF_RETRIES).await.unwrap();
    assert_eq!(outcome, CreateProofRequestOutcome::Replayed(explicit_id));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_get_nonexistent_returns_none() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let result = repo.get(Uuid::new_v4()).await.unwrap();
    assert!(result.is_none());
}

// ============================================================
// Guarded state transition tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_pending_to_running() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();

    let backend_id = format!("ptr-{}", Uuid::new_v4());
    let session_id = repo
        .transition_pending_to_running(CreateProofSession {
            proof_request_id: id,
            session_type: SessionType::Stark,
            backend_session_id: backend_id.clone(),
            metadata: None,
        })
        .await
        .unwrap();
    assert!(session_id.is_some());

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Running);

    let session =
        repo.get_session_by_backend_id(&backend_id).await.unwrap().expect("should find session");
    assert_eq!(session.status, SessionStatus::Running);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_pending_to_running_race() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();

    let first = repo
        .transition_pending_to_running(CreateProofSession {
            proof_request_id: id,
            session_type: SessionType::Stark,
            backend_session_id: format!("first-{}", Uuid::new_v4()),
            metadata: None,
        })
        .await
        .unwrap();
    assert!(first.is_some());

    let second = repo
        .transition_pending_to_running(CreateProofSession {
            proof_request_id: id,
            session_type: SessionType::Stark,
            backend_session_id: format!("second-{}", Uuid::new_v4()),
            metadata: None,
        })
        .await
        .unwrap();
    assert!(second.is_none(), "second transition should lose the race");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_pending_to_failed() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();

    let updated = repo.transition_pending_to_failed(id, "submission timeout".into()).await.unwrap();
    assert!(updated);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Failed);
    assert_eq!(req.error_message.as_deref(), Some("submission timeout"));
    assert!(req.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_pending_to_failed_wrong_state() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    // Still CREATED, not PENDING
    let updated = repo.transition_pending_to_failed(id, "should not work".into()).await.unwrap();
    assert!(!updated);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Created);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_running_to_failed() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    let updated =
        repo.transition_running_to_failed(id, Some("cluster timeout".into())).await.unwrap();
    assert!(updated);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Failed);
    assert_eq!(req.error_message.as_deref(), Some("cluster timeout"));
    assert!(req.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_transition_running_to_failed_wrong_state() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();
    // PENDING, not RUNNING
    let updated =
        repo.transition_running_to_failed(id, Some("should not work".into())).await.unwrap();
    assert!(!updated);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Pending);
}

// ============================================================
// Receipt update tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_update_receipt_if_running() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    // Intermediate receipt update while RUNNING succeeds and keeps status RUNNING.
    let updated = repo
        .update_receipt_if_running(UpdateReceipt {
            id,
            stark_receipt: Some(vec![1, 2, 3]),
            snark_receipt: None,
            status: ProofStatus::Running,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(updated);
    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Running);
    assert_eq!(req.stark_receipt.as_deref(), Some(&[1u8, 2, 3][..]));

    // A later intermediate update overwrites the receipt while still RUNNING.
    let updated = repo
        .update_receipt_if_running(UpdateReceipt {
            id,
            stark_receipt: Some(vec![4, 5, 6]),
            snark_receipt: None,
            status: ProofStatus::Running,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(updated);
    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.stark_receipt.as_deref(), Some(&[4u8, 5, 6][..]));

    // Once the request leaves RUNNING, intermediate updates are skipped.
    assert!(repo.transition_running_to_failed(id, Some("done".into())).await.unwrap());
    let updated = repo
        .update_receipt_if_running(UpdateReceipt {
            id,
            stark_receipt: Some(vec![7, 8, 9]),
            snark_receipt: None,
            status: ProofStatus::Running,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(!updated, "updates must be skipped once not RUNNING");
}

// ============================================================
// Atomic claim tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_atomic_claim_task() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();

    // First claim should succeed (CREATED -> PENDING)
    let claimed = repo.atomic_claim_task(id).await.unwrap();
    assert!(claimed);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Pending);

    // Second claim should fail (already PENDING)
    let claimed = repo.atomic_claim_task(id).await.unwrap();
    assert!(!claimed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_atomic_claim_nonexistent() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let claimed = repo.atomic_claim_task(Uuid::new_v4()).await.unwrap();
    assert!(!claimed);
}

// ============================================================
// Proof session tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_proof_session() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(compressed_request()).await.unwrap();
    let session_id = repo
        .create_proof_session(CreateProofSession {
            proof_request_id: req_id,
            session_type: SessionType::Stark,
            backend_session_id: format!("test-session-{}", Uuid::new_v4()),
            metadata: Some(serde_json::json!({"key": "value"})),
        })
        .await
        .unwrap();

    assert!(session_id > 0);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_get_session_by_backend_id() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(compressed_request()).await.unwrap();
    let backend_id = format!("backend-{}", Uuid::new_v4());

    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Stark,
        backend_session_id: backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    let session =
        repo.get_session_by_backend_id(&backend_id).await.unwrap().expect("should find session");
    assert_eq!(session.proof_request_id, req_id);
    assert_eq!(session.session_type, SessionType::Stark);
    assert_eq!(session.status, SessionStatus::Running);
    assert_eq!(session.backend_session_id, backend_id);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_get_sessions_for_request() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(snark_request()).await.unwrap();

    // Create STARK session
    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Stark,
        backend_session_id: format!("stark-{}", Uuid::new_v4()),
        metadata: None,
    })
    .await
    .unwrap();

    // Create SNARK session
    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Snark,
        backend_session_id: format!("snark-{}", Uuid::new_v4()),
        metadata: None,
    })
    .await
    .unwrap();

    let sessions = repo.get_sessions_for_request(req_id).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].session_type, SessionType::Stark);
    assert_eq!(sessions[1].session_type, SessionType::Snark);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_update_proof_session() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(compressed_request()).await.unwrap();
    let backend_id = format!("session-update-{}", Uuid::new_v4());

    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Stark,
        backend_session_id: backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    repo.update_proof_session(UpdateProofSession {
        backend_session_id: backend_id.clone(),
        status: SessionStatus::Completed,
        error_message: None,
        metadata: Some(serde_json::json!({"output_id": "abc123"})),
    })
    .await
    .unwrap();

    let session = repo.get_session_by_backend_id(&backend_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());
    assert_eq!(session.metadata.unwrap()["output_id"].as_str(), Some("abc123"));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_update_proof_session_if_non_terminal() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(compressed_request()).await.unwrap();
    let backend_id = format!("session-nonterminal-{}", Uuid::new_v4());

    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Stark,
        backend_session_id: backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    // First update: RUNNING -> COMPLETED (should succeed)
    let updated = repo
        .update_proof_session_if_non_terminal(UpdateProofSession {
            backend_session_id: backend_id.clone(),
            status: SessionStatus::Completed,
            error_message: None,
            metadata: None,
        })
        .await
        .unwrap();
    assert!(updated);

    // Second update: COMPLETED -> FAILED (should be skipped since COMPLETED is terminal)
    let updated = repo
        .update_proof_session_if_non_terminal(UpdateProofSession {
            backend_session_id: backend_id.clone(),
            status: SessionStatus::Failed,
            error_message: Some("late failure".into()),
            metadata: None,
        })
        .await
        .unwrap();
    assert!(!updated);

    let session = repo.get_session_by_backend_id(&backend_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Completed); // unchanged
}

// ============================================================
// Atomic transaction tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_fail_session_and_request() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (req_id, backend_id) = setup_running_request(&repo).await;

    let updated = repo
        .fail_session_and_request(&backend_id, req_id, Some("cluster timeout".into()))
        .await
        .unwrap();
    assert!(updated);

    let req = repo.get(req_id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Failed);
    assert_eq!(req.error_message.as_deref(), Some("cluster timeout"));
    assert!(req.completed_at.is_some());

    let session = repo.get_session_by_backend_id(&backend_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Failed);
    assert!(session.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_fail_session_and_request_skips_terminal() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (req_id, backend_id) = setup_running_request(&repo).await;

    repo.complete_session_and_update_receipt(
        &backend_id,
        UpdateReceipt {
            id: req_id,
            stark_receipt: Some(vec![0xDE, 0xAD]),
            snark_receipt: None,
            status: ProofStatus::Succeeded,
            error_message: None,
        },
    )
    .await
    .unwrap();

    // Now try to fail — request should NOT be updated (already SUCCEEDED, not RUNNING)
    let updated = repo
        .fail_session_and_request(&backend_id, req_id, Some("late error".into()))
        .await
        .unwrap();
    assert!(!updated);

    let req = repo.get(req_id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Succeeded);

    let session = repo.get_session_by_backend_id(&backend_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_complete_session_and_update_receipt() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (req_id, backend_id) = setup_running_request(&repo).await;

    let stark_data = vec![0xCA, 0xFE, 0xBA, 0xBE];
    let updated = repo
        .complete_session_and_update_receipt(
            &backend_id,
            UpdateReceipt {
                id: req_id,
                stark_receipt: Some(stark_data.clone()),
                snark_receipt: None,
                status: ProofStatus::Succeeded,
                error_message: None,
            },
        )
        .await
        .unwrap();
    assert!(updated);

    let req = repo.get(req_id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Succeeded);
    assert_eq!(req.stark_receipt.as_deref(), Some(stark_data.as_slice()));
    assert!(req.completed_at.is_some());

    let session = repo.get_session_by_backend_id(&backend_id).await.unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_complete_session_and_update_receipt_skips_non_running() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();
    // Request is PENDING, not RUNNING
    let backend_id = format!("complete-pending-{}", Uuid::new_v4());
    repo.create_proof_session(CreateProofSession {
        proof_request_id: id,
        session_type: SessionType::Stark,
        backend_session_id: backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    let updated = repo
        .complete_session_and_update_receipt(
            &backend_id,
            UpdateReceipt {
                id,
                stark_receipt: Some(vec![1, 2, 3]),
                snark_receipt: None,
                status: ProofStatus::Succeeded,
                error_message: None,
            },
        )
        .await
        .unwrap();
    assert!(!updated, "should not update a PENDING request");

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Pending); // unchanged
}

// ============================================================
// Retry logic tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_retry_or_fail_stuck_request_retries() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();
    sqlx::query(
        r#"
        UPDATE proof_requests
        SET stark_receipt = $1,
            snark_receipt = $2,
            result_payload = $3,
            submitted_by_worker_id = $4,
            submitted_lock_id = $5,
            completed_at = NOW()
        WHERE id = $6
        "#,
    )
    .bind(vec![0x01u8, 0x02u8])
    .bind(vec![0x03u8, 0x04u8])
    .bind(serde_json::json!({"proof_type": "compressed"}))
    .bind("stale-worker")
    .bind("stale-lock")
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck in PENDING").await.unwrap();
    assert_eq!(outcome, RetryOutcome::Retried);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Created);
    assert_eq!(req.retry_count, 1);
    assert!(req.error_message.is_none());
    assert!(req.stark_receipt.is_none());
    assert!(req.snark_receipt.is_none());
    assert!(req.result_payload.is_none());
    assert!(req.submitted_by_worker_id.is_none());
    assert!(req.submitted_lock_id.is_none());
    assert!(req.completed_at.is_none());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_retry_or_fail_stuck_request_retries_tee_request() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(tee_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();

    let stuck_requests = repo.get_stuck_requests(-1).await.unwrap();
    assert!(
        stuck_requests.iter().all(|request| request.id != id),
        "TEE requests should not enter legacy backend-session stuck detection"
    );

    let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck in PENDING").await.unwrap();
    assert_eq!(outcome, RetryOutcome::Retried);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Created);
    assert_eq!(req.retry_count, 1);
    assert!(req.proof_type.is_none());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_retry_or_fail_stuck_request_exhausted() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();

    // Retry 3 times: each cycle is claim → retry (resets to CREATED) → claim again
    for i in 0..3 {
        repo.atomic_claim_task(id).await.unwrap();
        let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck").await.unwrap();
        assert_eq!(outcome, RetryOutcome::Retried, "retry {i} should succeed");
    }

    // retry_count is now 3, claim once more
    repo.atomic_claim_task(id).await.unwrap();

    // This time should permanently fail (retry_count >= max_retries)
    let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck").await.unwrap();
    assert_eq!(outcome, RetryOutcome::PermanentlyFailed);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Failed);
    assert!(req.error_message.as_deref().unwrap().contains("max retries exceeded"));
    assert!(req.completed_at.is_some());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_retry_or_fail_stuck_request_wrong_state() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    // Request is RUNNING, not PENDING — should be skipped
    let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck").await.unwrap();
    assert_eq!(outcome, RetryOutcome::Skipped);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Running); // unchanged
}

// ============================================================
// Worker queue create tests
// ============================================================

/// `CREATED` -> `PENDING` -> `FAILED` (same transitions as a failed worker attempt).
async fn drive_to_failed(repo: &ProofRequestRepo, id: Uuid, error_message: &str) {
    assert!(repo.atomic_claim_task(id).await.unwrap(), "claim CREATED -> PENDING");
    assert!(
        repo.transition_pending_to_failed(id, error_message.into()).await.unwrap(),
        "transition PENDING -> FAILED",
    );
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_for_worker_queue_creates_claimable_job() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    drain_claimable_compressed_jobs(&repo).await;
    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string());

    let outcome = repo.create_for_worker_queue(req, TEST_MAX_PROOF_RETRIES).await.unwrap();
    assert!(matches!(outcome, CreateProofRequestOutcome::Created(id) if id == explicit_id));

    let job = repo
        .claim_next_proof_job(compressed_claim("worker-queue-create", 3))
        .await
        .unwrap()
        .expect("worker-created compressed job should be claimable");
    assert_eq!(job.id, explicit_id);
    assert_eq!(job.job_status, ProofJobStatus::Claimed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_for_worker_queue_accepts_tee_requests() {
    let repo = test_repo(test_pool().await);

    drain_claimable_tee_jobs(&repo).await;
    let explicit_id = format!("tee-worker-queue-{}", Uuid::new_v4());
    let mut req = tee_request();
    set_request_session_id(&mut req, explicit_id.clone());

    let outcome = repo.create_for_worker_queue(req, TEST_MAX_PROOF_RETRIES).await.unwrap();
    let id = outcome.id();

    let row = repo.get(id).await.unwrap().expect("TEE request should be stored");
    assert_eq!(row.session_id, explicit_id);
    assert_eq!(row.api_proof_type, ApiProofType::Tee);
    assert!(row.proof_type.is_none());

    let job = repo
        .claim_next_proof_job(tee_claim("tee-worker-queue-create", 3))
        .await
        .unwrap()
        .expect("worker-created TEE job should be claimable");
    assert_eq!(job.id, id);
    assert_eq!(job.job_status, ProofJobStatus::Claimed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_for_worker_queue_idempotent() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string());

    let first = repo.create_for_worker_queue(req.clone(), TEST_MAX_PROOF_RETRIES).await.unwrap();
    let second = repo.create_for_worker_queue(req, TEST_MAX_PROOF_RETRIES).await.unwrap();

    assert!(matches!(first, CreateProofRequestOutcome::Created(id) if id == explicit_id));
    assert!(matches!(second, CreateProofRequestOutcome::Replayed(id) if id == explicit_id));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_create_for_worker_queue_requeues_failed_row() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    set_request_session_id(&mut req, explicit_id.to_string());

    let first = repo.create_for_worker_queue(req.clone(), TEST_MAX_PROOF_RETRIES).await.unwrap();
    assert!(matches!(first, CreateProofRequestOutcome::Created(id) if id == explicit_id));
    drive_to_failed(&repo, explicit_id, "transient backend error").await;

    sqlx::query(
        "UPDATE proof_requests SET job_status = 'FAILED', worker_id = 'stale-worker', \
         lock_id = gen_random_uuid(), lock_expires_at = NOW(), claimed_at = NOW(), \
         last_heartbeat_at = NOW(), attempt = 4 WHERE id = $1",
    )
    .bind(explicit_id)
    .execute(&pool)
    .await
    .unwrap();

    let second = repo.create_for_worker_queue(req, TEST_MAX_PROOF_RETRIES).await.unwrap();
    assert!(matches!(second, CreateProofRequestOutcome::Requeued(id) if id == explicit_id));

    let after = repo.get(explicit_id).await.unwrap().unwrap();
    assert_eq!(after.status, ProofStatus::Created);
    assert_eq!(after.retry_count, 1);
    assert!(after.error_message.is_none());
    assert!(after.completed_at.is_none());

    let (job_status, attempt, worker_id): (String, i32, Option<String>) =
        sqlx::query_as("SELECT job_status, attempt, worker_id FROM proof_requests WHERE id = $1")
            .bind(explicit_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(job_status, "PENDING");
    assert_eq!(attempt, 0);
    assert!(worker_id.is_none());
}

// ============================================================
// Query tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_get_running_sessions() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let req_id = repo.create(compressed_request()).await.unwrap();
    let running_id = format!("running-session-{}", Uuid::new_v4());
    let completed_id = format!("completed-session-{}", Uuid::new_v4());

    // Create a running session
    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Stark,
        backend_session_id: running_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    // Create and complete another session
    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Snark,
        backend_session_id: completed_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();
    repo.update_proof_session(UpdateProofSession {
        backend_session_id: completed_id.clone(),
        status: SessionStatus::Completed,
        error_message: None,
        metadata: None,
    })
    .await
    .unwrap();

    let running = repo.get_running_sessions().await.unwrap();
    let has_running = running.iter().any(|s| s.backend_session_id == running_id);
    let has_completed = running.iter().any(|s| s.backend_session_id == completed_id);
    assert!(has_running, "should include running session");
    assert!(!has_completed, "should not include completed session");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_get_running_proof_requests() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    let running = repo.get_running_proof_requests().await.unwrap();
    let found = running.iter().any(|r| r.id == id);
    assert!(found, "should include our running request");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_list_with_filter() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id1 = repo.create(compressed_request()).await.unwrap();
    let (id2, _backend_id) = setup_running_request(&repo).await;

    // List only CREATED
    let created_list = repo.list(Some(ProofStatus::Created), 100).await.unwrap();
    let has_id1 = created_list.iter().any(|r| r.id == id1);
    let has_id2 = created_list.iter().any(|r| r.id == id2);
    assert!(has_id1, "CREATED request should be in list");
    assert!(!has_id2, "RUNNING request should not be in CREATED list");

    // List all
    let all_list = repo.list(None, 100).await.unwrap();
    assert!(all_list.iter().any(|r| r.id == id1));
    assert!(all_list.iter().any(|r| r.id == id2));
}

// ============================================================
// Two-stage SNARK pipeline test
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_full_snark_pipeline() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    // 1. Create SNARK request
    let req_id = repo.create(snark_request()).await.unwrap();

    // 2. Claim task (CREATED -> PENDING)
    assert!(repo.atomic_claim_task(req_id).await.unwrap());

    // 3. Submit STARK session (PENDING -> RUNNING)
    let stark_backend_id = format!("stark-pipeline-{}", Uuid::new_v4());
    let session_id = repo
        .transition_pending_to_running(CreateProofSession {
            proof_request_id: req_id,
            session_type: SessionType::Stark,
            backend_session_id: stark_backend_id.clone(),
            metadata: None,
        })
        .await
        .unwrap();
    assert!(session_id.is_some());

    // 4. STARK completes — store receipt but keep RUNNING (awaiting SNARK)
    let stark_receipt = vec![0x01, 0x02, 0x03];
    repo.complete_session_and_update_receipt(
        &stark_backend_id,
        UpdateReceipt {
            id: req_id,
            stark_receipt: Some(stark_receipt.clone()),
            snark_receipt: None,
            status: ProofStatus::Running, // still running — SNARK stage not done
            error_message: None,
        },
    )
    .await
    .unwrap();

    let req = repo.get(req_id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Running);
    assert_eq!(req.stark_receipt.as_deref(), Some(stark_receipt.as_slice()));
    assert!(req.snark_receipt.is_none());
    assert!(req.result_payload.is_none());

    // 5. Submit SNARK session
    let snark_backend_id = format!("snark-pipeline-{}", Uuid::new_v4());
    repo.create_proof_session(CreateProofSession {
        proof_request_id: req_id,
        session_type: SessionType::Snark,
        backend_session_id: snark_backend_id.clone(),
        metadata: None,
    })
    .await
    .unwrap();

    // 6. SNARK completes — store receipt, mark SUCCEEDED
    let snark_receipt = vec![0xAA, 0xBB, 0xCC];
    repo.complete_session_and_update_receipt(
        &snark_backend_id,
        UpdateReceipt {
            id: req_id,
            stark_receipt: None, // don't overwrite
            snark_receipt: Some(snark_receipt.clone()),
            status: ProofStatus::Succeeded,
            error_message: None,
        },
    )
    .await
    .unwrap();

    // 7. Verify final state
    let req = repo.get(req_id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Succeeded);
    assert_eq!(req.stark_receipt.as_deref(), Some(stark_receipt.as_slice())); // preserved
    assert_eq!(req.snark_receipt.as_deref(), Some(snark_receipt.as_slice()));
    let result_payload = req.result_payload.expect("SNARK result payload should be stored");
    let result: ProtocolProofResult =
        serde_json::from_value(result_payload).expect("SNARK result payload should deserialize");
    assert_eq!(
        result,
        ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: snark_receipt.into() }
        })
    );
    assert!(req.completed_at.is_some());

    let sessions = repo.get_sessions_for_request(req_id).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].session_type, SessionType::Stark);
    assert_eq!(sessions[0].status, SessionStatus::Completed);
    assert_eq!(sessions[1].session_type, SessionType::Snark);
    assert_eq!(sessions[1].status, SessionStatus::Completed);
}

// ============================================================
// Worker job claim tests (`claim_next_proof_job`)
// ============================================================

/// Build a claim with a long lease so claimed jobs stay out of the pool.
fn claim_job(
    worker_id: &str,
    api_proof_type: ApiProofType,
    tee_kinds: Vec<TeeKind>,
    zk_vms: Vec<ZkVmKind>,
    max_attempts: u32,
) -> ClaimProofJob {
    ClaimProofJob {
        worker_id: worker_id.to_owned(),
        api_proof_type,
        tee_kinds,
        zk_vms,
        lock_duration_seconds: 3600,
        max_attempts,
    }
}

/// A TEE worker claim advertising AWS Nitro capability.
fn tee_claim(worker_id: &str, max_attempts: u32) -> ClaimProofJob {
    claim_job(worker_id, ApiProofType::Tee, vec![TeeKind::AwsNitro], vec![], max_attempts)
}

/// A compressed ZK worker claim advertising SP1 capability.
fn compressed_claim(worker_id: &str, max_attempts: u32) -> ClaimProofJob {
    claim_job(worker_id, ApiProofType::Compressed, vec![], vec![ZkVmKind::Sp1], max_attempts)
}

/// Drain every currently claimable TEE job by claiming it under a long lease, so
/// each test starts from a known-empty TEE queue. Holding the jobs `CLAIMED` with
/// an unexpired lock (rather than completing them) keeps this independent of the
/// worker submit API.
async fn drain_claimable_tee_jobs(repo: &ProofRequestRepo) {
    while repo
        .claim_next_proof_job(tee_claim("drain-worker", u32::MAX))
        .await
        .expect("drain claim should not error")
        .is_some()
    {}
}

/// Drain every currently claimable compressed job for tests that need to claim a
/// freshly inserted ZK request deterministically.
async fn drain_claimable_compressed_jobs(repo: &ProofRequestRepo) {
    while repo
        .claim_next_proof_job(compressed_claim("drain-zk-worker", u32::MAX))
        .await
        .expect("drain claim should not error")
        .is_some()
    {}
}

/// Force a job's lock to appear expired so it becomes reclaimable.
async fn expire_lock(pool: &PgPool, id: Uuid) {
    sqlx::query(
        "UPDATE proof_requests SET lock_expires_at = NOW() - INTERVAL '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("expire lock should succeed");
}

fn compressed_result(bytes: Vec<u8>) -> ProtocolProofResult {
    ProtocolProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: bytes.into() })
}

fn uppercase_uuid_session_id() -> (Uuid, String) {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xaa;
    let id = Uuid::from_bytes(bytes);
    (id, id.to_string().to_uppercase())
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_claim_next_proof_job_claim_and_capabilities() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo).await;
    let id = repo.create(tee_request()).await.unwrap();

    // A worker advertising no matching capability claims nothing.
    let no_caps = claim_job("no-caps", ApiProofType::Tee, vec![], vec![], 3);
    assert!(repo.claim_next_proof_job(no_caps).await.unwrap().is_none());

    // A capable TEE worker claims the pending job and flips it to CLAIMED/RUNNING.
    let job = repo
        .claim_next_proof_job(tee_claim("worker-1", 3))
        .await
        .unwrap()
        .expect("the pending TEE job should be claimed");
    assert_eq!(job.id, id);
    assert_eq!(job.api_proof_type, ApiProofType::Tee);
    assert_eq!(job.job_status, ProofJobStatus::Claimed);
    assert_eq!(job.attempt, 1);
    assert_eq!(job.worker_id.as_deref(), Some("worker-1"));
    assert!(job.lock_id.is_some());
    assert!(job.lock_expires_at.is_some());
    assert!(job.claimed_at.is_some());
    assert!(job.last_heartbeat_at.is_some());
    assert_eq!(repo.get(id).await.unwrap().unwrap().status, ProofStatus::Running);

    // The TEE queue is now drained, and a TEE worker never claims a ZK job.
    repo.create(compressed_request()).await.unwrap();
    assert!(repo.claim_next_proof_job(tee_claim("worker-2", 3)).await.unwrap().is_none());

    // A ZK worker can claim a compressed job (block-number ordering may surface a
    // lower-block pending ZK job from another test, so we only assert the proof type).
    let zk = claim_job("zk-worker", ApiProofType::Compressed, vec![], vec![ZkVmKind::Sp1], 3);
    let job = repo
        .claim_next_proof_job(zk)
        .await
        .unwrap()
        .expect("a compressed job should be claimable by a ZK worker");
    assert_eq!(job.api_proof_type, ApiProofType::Compressed);
    assert_eq!(job.job_status, ProofJobStatus::Claimed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_claim_next_proof_job_concurrent_workers_never_double_claim() {
    let pool = test_pool().await;
    let repo_a = test_repo(pool.clone());
    let repo_b = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo_a).await;
    let id = repo_a.create(tee_request()).await.unwrap();

    let (a, b) = tokio::join!(
        repo_a.claim_next_proof_job(tee_claim("worker-a", 3)),
        repo_b.claim_next_proof_job(tee_claim("worker-b", 3)),
    );

    // Exactly one worker wins the single available job.
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!([&a, &b].into_iter().filter(|j| j.is_some()).count(), 1);
    let winner = a.or(b).unwrap();
    assert_eq!(winner.id, id);
    assert_eq!(winner.attempt, 1);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_claim_next_proof_job_orders_by_start_block_number() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    drain_claimable_compressed_jobs(&repo).await;
    let high_block_id = repo.create(compressed_request_at(200)).await.unwrap();
    let low_block_id = repo.create(compressed_request_at(100)).await.unwrap();

    let job = repo
        .claim_next_proof_job(compressed_claim("block-order-worker", 3))
        .await
        .unwrap()
        .expect("a compressed job should be claimable");

    assert_eq!(job.id, low_block_id);
    assert_ne!(job.id, high_block_id);
    assert_eq!(job.api_proof_type, ApiProofType::Compressed);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_claim_next_proof_job_expired_lock_lifecycle() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo).await;
    let id = repo.create(tee_request()).await.unwrap();

    // First claim, then expire the lock so it becomes reclaimable.
    let first = repo
        .claim_next_proof_job(tee_claim("worker-a", 2))
        .await
        .unwrap()
        .expect("first claim should succeed");
    assert_eq!(first.attempt, 1);
    expire_lock(&pool, id).await;

    // Reclaim issues a fresh lock and increments the attempt (1 < max_attempts 2).
    let second = repo
        .claim_next_proof_job(tee_claim("worker-b", 2))
        .await
        .unwrap()
        .expect("expired lock should be reclaimable");
    assert_eq!(second.id, id);
    assert_eq!(second.attempt, 2);
    assert_eq!(second.worker_id.as_deref(), Some("worker-b"));
    assert_ne!(second.lock_id, first.lock_id, "a fresh fencing token is issued");

    // Once attempts are exhausted (2 == max_attempts), an expired lock is not reclaimed.
    expire_lock(&pool, id).await;
    assert!(
        repo.claim_next_proof_job(tee_claim("worker-c", 2)).await.unwrap().is_none(),
        "exhausted job must not be reclaimed"
    );
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_heartbeat_proof_job_guards_current_expired_and_reclaimed_locks() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo).await;
    let (explicit_id, uppercase_session_id) = uppercase_uuid_session_id();
    let mut request = tee_request();
    set_request_session_id(&mut request, uppercase_session_id.clone());
    let id = repo.create(request).await.unwrap();
    assert_eq!(id, explicit_id);
    let first = repo
        .claim_next_proof_job(tee_claim("first-worker", 3))
        .await
        .unwrap()
        .expect("first claim should succeed");
    assert_eq!(first.id, id);
    let first_lock = first.lock_id.expect("first claim has lock");

    let updated = repo
        .heartbeat_proof_job(HeartbeatProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id: first_lock,
            worker_id: "first-worker".to_owned(),
            lock_duration_seconds: 7200,
        })
        .await
        .unwrap();
    let HeartbeatOutcome::Updated(updated) = updated else {
        panic!("heartbeat should update the current lock");
    };
    assert_eq!(updated.id, id);
    assert_eq!(updated.lock_id, Some(first_lock));
    assert_eq!(updated.worker_id.as_deref(), Some("first-worker"));
    assert!(updated.lock_expires_at >= first.lock_expires_at);

    let stale = repo
        .heartbeat_proof_job(HeartbeatProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id: Uuid::new_v4(),
            worker_id: "first-worker".to_owned(),
            lock_duration_seconds: 7200,
        })
        .await
        .unwrap();
    assert!(matches!(stale, HeartbeatOutcome::StaleLock(_)));

    expire_lock(&pool, id).await;

    let expired = repo
        .heartbeat_proof_job(HeartbeatProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id: first_lock,
            worker_id: "first-worker".to_owned(),
            lock_duration_seconds: 3600,
        })
        .await
        .unwrap();
    assert!(matches!(expired, HeartbeatOutcome::Expired(_)));

    let second = repo
        .claim_next_proof_job(tee_claim("second-worker", 3))
        .await
        .unwrap()
        .expect("expired lock should be reclaimed");
    assert_ne!(second.lock_id, Some(first_lock));

    let stale = repo
        .heartbeat_proof_job(HeartbeatProofJob {
            session_id: uppercase_session_id,
            lock_id: first_lock,
            worker_id: "first-worker".to_owned(),
            lock_duration_seconds: 3600,
        })
        .await
        .unwrap();
    assert!(matches!(stale, HeartbeatOutcome::StaleLock(_)));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_complete_claimed_proof_job_guards_and_stores_result() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    drain_claimable_compressed_jobs(&repo).await;
    let (explicit_id, uppercase_session_id) = uppercase_uuid_session_id();
    let mut request = compressed_request();
    set_request_session_id(&mut request, uppercase_session_id.clone());
    let id = repo.create(request).await.unwrap();
    assert_eq!(id, explicit_id);
    let claimed = repo
        .claim_next_proof_job(compressed_claim("submit-worker", 3))
        .await
        .unwrap()
        .expect("compressed job should be claimed");
    assert_eq!(claimed.id, id);
    let lock_id = claimed.lock_id.expect("claimed job has lock");

    let stale = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id: Uuid::new_v4(),
            worker_id: "submit-worker".to_owned(),
            result: compressed_result(vec![0xde, 0xad]),
        })
        .await
        .unwrap();
    assert!(matches!(stale, SubmitProofOutcome::StaleLock(_)));
    assert!(repo.get(id).await.unwrap().unwrap().result_payload.is_none());

    let result = compressed_result(vec![0xca, 0xfe]);
    let submitted = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id,
            worker_id: "submit-worker".to_owned(),
            result: result.clone(),
        })
        .await
        .unwrap();
    let SubmitProofOutcome::Completed(completed) = submitted else {
        panic!("submit should complete the current claim");
    };
    assert_eq!(completed.job_status, ProofJobStatus::Succeeded);
    assert!(completed.completed_at.is_some());

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Succeeded);
    assert_eq!(req.stark_receipt.as_deref(), Some(&[0xca, 0xfe][..]));
    assert!(req.snark_receipt.is_none());
    assert_eq!(req.submitted_by_worker_id.as_deref(), Some("submit-worker"));
    assert_eq!(req.submitted_lock_id, Some(lock_id.to_string()));
    let stored: ProtocolProofResult =
        serde_json::from_value(req.result_payload.expect("result payload should be stored"))
            .expect("stored result should deserialize");
    assert_eq!(stored, result);

    // An identical retry from the same worker/lock is idempotent.
    let replay = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id,
            worker_id: "submit-worker".to_owned(),
            result: result.clone(),
        })
        .await
        .unwrap();
    let SubmitProofOutcome::Completed(replayed) = replay else {
        panic!("identical retry should be idempotent");
    };
    assert_eq!(replayed.job_status, ProofJobStatus::Succeeded);
    assert_eq!(
        repo.get(id).await.unwrap().unwrap().stark_receipt.as_deref(),
        Some(&[0xca, 0xfe][..])
    );

    // Same worker/lock, different payload: conflict, stored result kept.
    let conflict = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: uppercase_session_id.clone(),
            lock_id,
            worker_id: "submit-worker".to_owned(),
            result: compressed_result(vec![0xba, 0xad]),
        })
        .await
        .unwrap();
    assert!(matches!(conflict, SubmitProofOutcome::ResultConflict { .. }));
    assert_eq!(
        repo.get(id).await.unwrap().unwrap().stark_receipt.as_deref(),
        Some(&[0xca, 0xfe][..])
    );

    // A retry that no longer owns the lock still sees a terminal job.
    let foreign = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: uppercase_session_id,
            lock_id: Uuid::new_v4(),
            worker_id: "submit-worker".to_owned(),
            result: result.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(foreign, SubmitProofOutcome::Terminal(_)));
    assert_eq!(
        repo.get(id).await.unwrap().unwrap().stark_receipt.as_deref(),
        Some(&[0xca, 0xfe][..])
    );
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_complete_claimed_proof_job_rejects_mismatched_result() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    drain_claimable_compressed_jobs(&repo).await;
    let id = repo.create(compressed_request()).await.unwrap();
    let claimed = repo
        .claim_next_proof_job(compressed_claim("mismatch-worker", 3))
        .await
        .unwrap()
        .expect("compressed job should be claimed");
    let lock_id = claimed.lock_id.expect("claimed job has lock");

    // Non-owners are rejected before result-type validation, so mismatched
    // submissions do not expose the job's expected proof result shape.
    let stale_mismatch = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: claimed.session_id.clone(),
            lock_id: Uuid::new_v4(),
            worker_id: "non-owner".to_owned(),
            result: ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![0x01].into() },
            }),
        })
        .await
        .unwrap();
    assert!(matches!(stale_mismatch, SubmitProofOutcome::StaleLock(_)));

    // A valid lock for a compressed job must not store a SNARK result.
    let mismatch = repo
        .complete_claimed_proof_job(CompleteClaimedProofJob {
            session_id: claimed.session_id.clone(),
            lock_id,
            worker_id: "mismatch-worker".to_owned(),
            result: ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![0x01].into() },
            }),
        })
        .await
        .unwrap();
    assert!(matches!(mismatch, SubmitProofOutcome::ResultMismatch { .. }));

    // The job is left untouched.
    let job = repo
        .get_proof_job_by_session_id(&claimed.session_id)
        .await
        .unwrap()
        .expect("job still exists");
    assert_eq!(job.job_status, ProofJobStatus::Claimed);
    assert!(repo.get(id).await.unwrap().unwrap().result_payload.is_none());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_fail_expired_proof_jobs_enforces_retry_exhaustion() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo).await;
    let id = repo.create(tee_request()).await.unwrap();
    let first = repo
        .claim_next_proof_job(tee_claim("retry-worker-a", 2))
        .await
        .unwrap()
        .expect("first claim should succeed");
    expire_lock(&pool, id).await;

    let failed = repo
        .fail_expired_proof_jobs(FailExpiredProofJobs {
            max_attempts: 2,
            batch_size: 100,
            error_message: "worker lock expired after retry budget",
        })
        .await
        .unwrap();
    assert!(failed.iter().all(|job| job.id != id), "attempt 1 is still under the retry budget");

    let second = repo
        .claim_next_proof_job(tee_claim("retry-worker-b", 2))
        .await
        .unwrap()
        .expect("second claim should succeed before exhaustion");
    assert_eq!(second.id, id);
    assert_eq!(second.attempt, 2);
    assert_ne!(second.lock_id, first.lock_id);
    expire_lock(&pool, id).await;

    let failed = repo
        .fail_expired_proof_jobs(FailExpiredProofJobs {
            max_attempts: 2,
            batch_size: 100,
            error_message: "worker lock expired after retry budget",
        })
        .await
        .unwrap();
    let job = failed.iter().find(|job| job.id == id).expect("exhausted job should fail");
    assert_eq!(job.job_status, ProofJobStatus::Failed);
    assert!(job.completed_at.is_some());
    assert_eq!(job.error_message.as_deref(), Some("worker lock expired after retry budget"));

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Failed);
    assert_eq!(req.error_message.as_deref(), Some("worker lock expired after retry budget"));
    assert!(req.completed_at.is_some());

    let terminal = repo
        .heartbeat_proof_job(HeartbeatProofJob {
            session_id: second.session_id,
            lock_id: second.lock_id.expect("second claim has lock"),
            worker_id: "retry-worker-b".to_owned(),
            lock_duration_seconds: 3600,
        })
        .await
        .unwrap();
    assert!(matches!(terminal, HeartbeatOutcome::Terminal(_)));
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-prover-service-db --test postgres_integration --test-threads=1`"]
async fn test_fail_expired_proof_jobs_honors_batch_size() {
    let pool = test_pool().await;
    let repo = test_repo(pool.clone());

    drain_claimable_tee_jobs(&repo).await;
    let ids = [
        repo.create(tee_request()).await.unwrap(),
        repo.create(tee_request()).await.unwrap(),
        repo.create(tee_request()).await.unwrap(),
    ];

    for _ in ids {
        let claimed = repo
            .claim_next_proof_job(tee_claim("batch-reaper-worker", 1))
            .await
            .unwrap()
            .expect("pending job should be claimed");
        assert!(ids.contains(&claimed.id));
        expire_lock(&pool, claimed.id).await;
    }

    let first_batch = repo
        .fail_expired_proof_jobs(FailExpiredProofJobs {
            max_attempts: 1,
            batch_size: 2,
            error_message: "worker lock expired after retry budget",
        })
        .await
        .unwrap();
    assert_eq!(first_batch.len(), 2);
    assert!(first_batch.iter().all(|job| ids.contains(&job.id)));

    let second_batch = repo
        .fail_expired_proof_jobs(FailExpiredProofJobs {
            max_attempts: 1,
            batch_size: 2,
            error_message: "worker lock expired after retry budget",
        })
        .await
        .unwrap();
    assert_eq!(second_batch.len(), 1);
    assert!(ids.contains(&second_batch[0].id));

    let final_batch = repo
        .fail_expired_proof_jobs(FailExpiredProofJobs {
            max_attempts: 1,
            batch_size: 2,
            error_message: "worker lock expired after retry budget",
        })
        .await
        .unwrap();
    assert!(final_batch.is_empty());
}
