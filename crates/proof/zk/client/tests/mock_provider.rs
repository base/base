//! Integration test demonstrating that `ZkProofProvider` can be mocked for
//! testing without needing a real gRPC server.

use async_trait::async_trait;
use base_zk_client::{
    GetProofRequest, GetProofResponse, ProveBlockRequest, ProveBlockResponse, Status, ZkProofError,
    ZkProofProvider,
};

/// A mock implementation of [`ZkProofProvider`] that returns canned responses.
struct MockZkProvider;

#[async_trait]
impl ZkProofProvider for MockZkProvider {
    async fn prove_block(
        &self,
        _request: ProveBlockRequest,
    ) -> Result<ProveBlockResponse, ZkProofError> {
        Ok(ProveBlockResponse {
            session_id: "mock-session-123".into(),
            status: Status::Pending.into(),
        })
    }

    async fn get_proof(&self, _request: GetProofRequest) -> Result<GetProofResponse, ZkProofError> {
        Ok(GetProofResponse {
            status: Status::Completed.into(),
            proof: vec![0xDE, 0xAD, 0xBE, 0xEF],
            error_message: String::new(),
        })
    }
}

/// Verify that [`prove_block`] returns the expected canned session ID and
/// pending status.
#[tokio::test]
async fn mock_prove_block_returns_session_id() {
    let provider = MockZkProvider;

    let request = ProveBlockRequest {
        l1_head: vec![0u8; 32],
        agreed_l2_head_hash: vec![0u8; 32],
        agreed_l2_output_root: vec![0u8; 32],
        claimed_l2_output_root: vec![0u8; 32],
        claimed_l2_block_number: 100,
        ..Default::default()
    };

    let response = provider.prove_block(request).await.expect("prove_block should succeed");

    assert_eq!(response.session_id, "mock-session-123");
    assert_eq!(response.status, i32::from(Status::Pending));
}

/// Verify that [`get_proof`] returns a completed status with proof bytes.
#[tokio::test]
async fn mock_get_proof_returns_completed() {
    let provider = MockZkProvider;

    let request = GetProofRequest { session_id: "mock-session-123".into() };

    let response = provider.get_proof(request).await.expect("get_proof should succeed");

    assert_eq!(response.status, i32::from(Status::Completed));
    assert_eq!(response.proof, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(response.error_message.is_empty());
}

/// Verify that a mock can return errors and that `is_retryable` classifies them
/// correctly.
#[tokio::test]
async fn error_retryability() {
    let connection_err = ZkProofError::Connection("connection refused".into());
    assert!(connection_err.is_retryable());

    let timeout_err = ZkProofError::Timeout("deadline exceeded".into());
    assert!(timeout_err.is_retryable());

    let invalid_url_err = ZkProofError::InvalidUrl("not a url".into());
    assert!(!invalid_url_err.is_retryable());

    let unavailable = ZkProofError::GrpcStatus(tonic::Status::unavailable("service down"));
    assert!(unavailable.is_retryable());

    let not_found = ZkProofError::GrpcStatus(tonic::Status::not_found("session gone"));
    assert!(!not_found.is_retryable());

    let invalid_arg = ZkProofError::GrpcStatus(tonic::Status::invalid_argument("bad request"));
    assert!(!invalid_arg.is_retryable());
}

/// Verify that a provider can be used as a trait object behind `Box<dyn ZkProofProvider>`.
#[tokio::test]
async fn trait_object_usage() {
    let provider: Box<dyn ZkProofProvider> = Box::new(MockZkProvider);

    let request = ProveBlockRequest::default();
    let response = provider.prove_block(request).await.expect("prove_block should succeed");

    assert_eq!(response.session_id, "mock-session-123");
}
