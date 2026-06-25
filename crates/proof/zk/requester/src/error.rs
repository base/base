//! Error types for ZK proof requester flows.

use base_prover_service_client::ProverServiceClientError;
use base_prover_service_protocol::{ProofType, ProveBlockRangeResponse};
use thiserror::Error;

/// Errors returned while requesting ZK proofs from the prover service.
#[derive(Debug, Error)]
pub enum ZkProofRequesterError {
    /// Prover-service requester RPC failed.
    #[error("prover-service requester call failed: {0}")]
    Client(#[from] ProverServiceClientError),
    /// Aggregation request failed after range request acceptance.
    #[error(
        "aggregation request failed after range request {range_session_id} was accepted: {source}"
    )]
    AggregationRequestFailed {
        /// Accepted range proof session.
        range: ProveBlockRangeResponse,
        /// Accepted range proof session identifier.
        range_session_id: String,
        /// Aggregation request failure.
        #[source]
        source: ProverServiceClientError,
    },
    /// A proof request failed before producing a result.
    #[error("proof request {session_id} failed: {message}")]
    ProofFailed {
        /// Prover-service session identifier.
        session_id: String,
        /// Failure message returned by prover-service.
        message: String,
    },
    /// A proof request succeeded without a result payload.
    #[error("proof request {session_id} succeeded without a result")]
    MissingResult {
        /// Prover-service session identifier.
        session_id: String,
    },
    /// A proof request returned a different result type than the caller expected.
    #[error("proof request {session_id} returned {actual:?}; expected {expected:?}")]
    UnexpectedResult {
        /// Prover-service session identifier.
        session_id: String,
        /// Expected result type.
        expected: ProofType,
        /// Actual result type.
        actual: ProofType,
    },
}
