//! Integration test demonstrating that `ZkProofProvider` can be mocked for
//! testing without needing a real gRPC server.

use async_trait::async_trait;
use base_zk_client::{
    GetProofRequest, GetProofResponse, ProofJobStatus, ProofType, ProveBlockRequest,
    ProveBlockResponse, ZkProofError, ZkProofProvider,
};
use rstest::rstest;

/// A mock implementation of [`ZkProofProvider`] that returns canned responses.
struct MockZkProvider;

#[async_trait]
impl ZkProofProvider for MockZkProvider {
    async fn prove_block(
        &self,
        _request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        Ok(ProveBlockResponse { session_id: "mock-session-123".into() })
    }

    async fn get_proof(&self, _request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        Ok(GetProofResponse {
            status: ProofJobStatus::Succeeded.into(),
            receipt: vec![0xDE, 0xAD, 0xBE, 0xEF],
            error_message: None,
        })
    }
}

/// A mock that always returns errors, demonstrating error-path mockability.
struct FailingMockProvider;

#[async_trait]
impl ZkProofProvider for FailingMockProvider {
    async fn prove_block(
        &self,
        _request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        Err(ZkProofError::GrpcStatus(tonic::Status::unavailable("mock connection refused")))
    }

    async fn get_proof(&self, _request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        Err(ZkProofError::GrpcStatus(tonic::Status::unavailable("mock service down")))
    }
}

/// Verify that [`prove_block`] returns the expected canned session ID and
/// pending status.
#[tokio::test]
async fn mock_prove_block_returns_session_id() {
    let provider = MockZkProvider;

    let request = ProveBlockRequest {
        start_block_number: 100,
        number_of_blocks_to_prove: 1,
        sequence_window: None,
        proof_type: ProofType::Compressed.into(),
        session_id: None,
        prover_address: None,
        l1_head: None,
    };

    let response = provider.prove_block(request).await.expect("prove_block should succeed");

    assert_eq!(response.session_id, "mock-session-123");
}

/// Verify that [`get_proof`] returns a completed status with proof bytes.
#[tokio::test]
async fn mock_get_proof_returns_completed() {
    let provider = MockZkProvider;

    let request = GetProofRequest { session_id: "mock-session-123".into(), receipt_type: None };

    let response = provider.get_proof(request).await.expect("get_proof should succeed");

    assert_eq!(response.status, i32::from(ProofJobStatus::Succeeded));
    assert_eq!(response.receipt, vec![0xDE, 0xAD, 0xBE, 0xEF]);
}

/// Verify that `is_retryable` classifies all error variants correctly,
/// including every retryable gRPC status code.
#[rstest]
#[case::invalid_url(ZkProofError::InvalidUrl("not a url".into()), false)]
#[case::grpc_unavailable(
    ZkProofError::GrpcStatus(tonic::Status::unavailable("service down")),
    true
)]
#[case::grpc_deadline_exceeded(
    ZkProofError::GrpcStatus(tonic::Status::deadline_exceeded("timeout")),
    true
)]
#[case::grpc_resource_exhausted(
    ZkProofError::GrpcStatus(tonic::Status::resource_exhausted("rate limited")),
    true
)]
#[case::grpc_aborted(
    ZkProofError::GrpcStatus(tonic::Status::aborted("transaction conflict")),
    true
)]
#[case::grpc_not_found(ZkProofError::GrpcStatus(tonic::Status::not_found("session gone")), false)]
#[case::grpc_invalid_argument(
    ZkProofError::GrpcStatus(tonic::Status::invalid_argument("bad request")),
    false
)]
fn error_retryability(#[case] error: ZkProofError, #[case] expected: bool) {
    assert_eq!(error.is_retryable(), expected);
}

/// Verify the `From<tonic::Status>` conversion produces a retryable error.
#[test]
fn grpc_status_from_conversion() {
    let error: ZkProofError = tonic::Status::unavailable("test").into();
    assert!(error.is_retryable());
}

/// Verify that a provider can be used as a trait object behind `Box<dyn ZkProofProvider>`.
#[tokio::test]
async fn trait_object_usage() {
    let provider: Box<dyn ZkProofProvider> = Box::new(MockZkProvider);

    let request = ProveBlockRequest::default();
    let response = provider.prove_block(request).await.expect("prove_block should succeed");

    assert_eq!(response.session_id, "mock-session-123");
}

/// Verify that errors propagate correctly through the trait when a mock fails.
#[tokio::test]
async fn failing_mock_propagates_errors() {
    let provider = FailingMockProvider;

    let prove_err = provider
        .prove_block(ProveBlockRequest::default())
        .await
        .expect_err("prove_block should fail");
    assert!(matches!(prove_err, ZkProofError::GrpcStatus(_)));

    let get_err = provider
        .get_proof(GetProofRequest { session_id: "any".into(), receipt_type: None })
        .await
        .expect_err("get_proof should fail");
    assert!(matches!(get_err, ZkProofError::GrpcStatus(_)));
}
