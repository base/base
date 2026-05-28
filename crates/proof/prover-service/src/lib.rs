#![doc = include_str!("../README.md")]

mod proto {
    tonic::include_proto!("prover");
}

#[cfg(feature = "server")]
pub use proto::prover_service_server;

/// Serialized protobuf `FileDescriptorSet` for the prover service.
#[cfg(feature = "server")]
pub const PROVER_SERVICE_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("prover_service_descriptor");

pub use proto::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    ExecutionStats, FailProofJobRequest, FailProofJobResponse, GetProofJobRequest,
    GetProofJobResponse, GetProofRequest, GetProofResponse, HeartbeatProofJobRequest,
    HeartbeatProofJobResponse, ListProofsRequest, ListProofsResponse, ProofJob, ProofRequest,
    ProofResult, ProofSummary, ProofType, ReceiptType, SubmitProofRequest, SubmitProofResponse,
    TeeProofRequest, TeeProofResult, TeeProposal, ZkProofRequest, ZkProofResult,
    get_proof_response, proof_job, proof_request, proof_result, prover_service_client,
};
