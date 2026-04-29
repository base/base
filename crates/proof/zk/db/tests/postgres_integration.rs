//! Integration tests for [`ProofRequestRepo`] against a real `PostgreSQL` database.
//!
//! These tests require a running Postgres instance with the prover schema applied.
//!
//! Run with:
//!   ```sh
//!   DATABASE_URL=postgres://prover:prover@localhost:5433/prover \
//!     cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1
//!   ```
//!
//! Tests are marked `#[ignore]` so they're skipped by default (no Postgres in CI)
//! and must be opted into explicitly via `--run-ignored all`.
//! They run sequentially (`--test-threads=1`) because they share the same database;
//! each test creates unique UUIDs so they don't collide.

use std::time::Duration;

use base_zk_db::{
    CreateProofRequest, CreateProofSession, MarkOutboxError, MarkOutboxProcessed, ProofRequestRepo,
    ProofStatus, ProofType, RetryOutcome, SessionStatus, SessionType, UpdateProofSession,
    UpdateReceipt,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

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

const fn compressed_request() -> CreateProofRequest {
    CreateProofRequest {
        start_block_number: 100,
        number_of_blocks_to_prove: 5,
        sequence_window: Some(50),
        proof_type: ProofType::OpSuccinctSp1ClusterCompressed,
        session_id: None,
        prover_address: None,
        l1_head: None,
    }
}

fn snark_request() -> CreateProofRequest {
    CreateProofRequest {
        start_block_number: 200,
        number_of_blocks_to_prove: 10,
        sequence_window: Some(100),
        proof_type: ProofType::OpSuccinctSp1ClusterSnarkGroth16,
        session_id: None,
        prover_address: Some("0x1234567890abcdef1234567890abcdef12345678".to_string()),
        l1_head: Some(
            "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string(),
        ),
    }
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_create_and_get_compressed() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    let req = repo.get(id).await.unwrap().expect("should find request");

    assert_eq!(req.id, id);
    assert_eq!(req.start_block_number, 100);
    assert_eq!(req.number_of_blocks_to_prove, 5);
    assert_eq!(req.sequence_window, Some(50));
    assert_eq!(req.proof_type, ProofType::OpSuccinctSp1ClusterCompressed);
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_create_and_get_snark() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(snark_request()).await.unwrap();
    let req = repo.get(id).await.unwrap().expect("should find request");

    assert_eq!(req.start_block_number, 200);
    assert_eq!(req.number_of_blocks_to_prove, 10);
    assert_eq!(req.proof_type, ProofType::OpSuccinctSp1ClusterSnarkGroth16);
    assert_eq!(req.prover_address.as_deref(), Some("0x1234567890abcdef1234567890abcdef12345678"));
    assert_eq!(
        req.l1_head.as_deref(),
        Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
    );
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_create_with_session_id() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    req.session_id = Some(explicit_id);

    let id = repo.create(req).await.unwrap();
    assert_eq!(id, explicit_id);

    let fetched = repo.get(id).await.unwrap().expect("should find request");
    assert_eq!(fetched.id, explicit_id);
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_update_receipt_if_running() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    let updated = repo
        .update_receipt_if_running(UpdateReceipt {
            id,
            stark_receipt: Some(vec![1, 2, 3]),
            snark_receipt: None,
            status: ProofStatus::Succeeded,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(updated);

    // Now that it's SUCCEEDED, a second update should be skipped
    let updated = repo
        .update_receipt_if_running(UpdateReceipt {
            id,
            stark_receipt: Some(vec![4, 5, 6]),
            snark_receipt: None,
            status: ProofStatus::Succeeded,
            error_message: None,
        })
        .await
        .unwrap();
    assert!(!updated);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.stark_receipt.as_deref(), Some(&[1u8, 2, 3][..])); // first write won
}

// ============================================================
// Atomic claim tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_retry_or_fail_stuck_request_retries() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create(compressed_request()).await.unwrap();
    repo.atomic_claim_task(id).await.unwrap();

    let outcome = repo.retry_or_fail_stuck_request(id, 3, "stuck in PENDING").await.unwrap();
    assert_eq!(outcome, RetryOutcome::Retried);

    let req = repo.get(id).await.unwrap().unwrap();
    assert_eq!(req.status, ProofStatus::Created);
    assert_eq!(req.retry_count, 1);
    assert!(req.error_message.is_none());
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
// Outbox tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_create_with_outbox() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create_with_outbox(compressed_request()).await.unwrap();

    // Verify proof request exists
    let req = repo.get(id).await.unwrap().expect("should find request");
    assert_eq!(req.status, ProofStatus::Created);

    // Verify outbox entry was created
    let entries = repo.get_unprocessed_outbox_entries(100, 3).await.unwrap();
    let found = entries.iter().any(|e| e.proof_request_id == id);
    assert!(found, "outbox entry should exist for this request");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_create_with_outbox_idempotent() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let explicit_id = Uuid::new_v4();
    let mut req = compressed_request();
    req.session_id = Some(explicit_id);

    let id1 = repo.create_with_outbox(req.clone()).await.unwrap();
    let id2 = repo.create_with_outbox(req).await.unwrap();

    assert_eq!(id1, explicit_id);
    assert_eq!(id2, explicit_id); // idempotent — same ID returned
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_outbox_process_and_cleanup() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create_with_outbox(compressed_request()).await.unwrap();

    // Get unprocessed entries
    let entries = repo.get_unprocessed_outbox_entries(10, 3).await.unwrap();
    let entry = entries.iter().find(|e| e.proof_request_id == id).expect("should find our entry");
    assert!(!entry.processed);
    let seq = entry.sequence_id;

    // Mark processed
    repo.mark_outbox_processed(MarkOutboxProcessed { sequence_id: seq }).await.unwrap();

    // Should no longer appear in unprocessed
    let entries = repo.get_unprocessed_outbox_entries(100, 3).await.unwrap();
    let found = entries.iter().any(|e| e.proof_request_id == id);
    assert!(!found, "processed entry should not appear in unprocessed");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_outbox_error_tracking() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let id = repo.create_with_outbox(compressed_request()).await.unwrap();

    let entries = repo.get_unprocessed_outbox_entries(100, 3).await.unwrap();
    let entry = entries.iter().find(|e| e.proof_request_id == id).expect("should find our entry");
    let seq = entry.sequence_id;
    assert_eq!(entry.retry_count, 0);

    // Record errors
    repo.mark_outbox_error(MarkOutboxError {
        sequence_id: seq,
        error_message: "first error".into(),
    })
    .await
    .unwrap();

    repo.mark_outbox_error(MarkOutboxError {
        sequence_id: seq,
        error_message: "second error".into(),
    })
    .await
    .unwrap();

    // Verify retry count incremented
    let entries = repo.get_unprocessed_outbox_entries(100, 3).await.unwrap();
    let entry = entries.iter().find(|e| e.sequence_id == seq).expect("should still find entry");
    assert_eq!(entry.retry_count, 2);
    assert_eq!(entry.last_error.as_deref(), Some("second error"));
}

// ============================================================
// Query tests
// ============================================================

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
async fn test_get_running_proof_requests() {
    let pool = test_pool().await;
    let repo = test_repo(pool);

    let (id, _backend_id) = setup_running_request(&repo).await;

    let running = repo.get_running_proof_requests().await.unwrap();
    let found = running.iter().any(|r| r.id == id);
    assert!(found, "should include our running request");
}

#[tokio::test]
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
#[ignore = "requires a running Postgres with the prover schema (set DATABASE_URL); run with `cargo nextest run --run-ignored all -p base-zk-db --test postgres_integration --test-threads=1`"]
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
    assert!(req.completed_at.is_some());

    let sessions = repo.get_sessions_for_request(req_id).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].session_type, SessionType::Stark);
    assert_eq!(sessions[0].status, SessionStatus::Completed);
    assert_eq!(sessions[1].session_type, SessionType::Snark);
    assert_eq!(sessions[1].status, SessionStatus::Completed);
}
