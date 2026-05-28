#![doc = include_str!("../README.md")]

#[allow(
    unreachable_pub,
    clippy::clone_on_ref_ptr,
    clippy::derive_partial_eq_without_eq,
    clippy::doc_markdown,
    clippy::missing_const_for_fn
)]
mod proto {
    tonic::include_proto!("prover_service");
}

#[cfg(feature = "server")]
pub use proto::prover_service_server;

/// Serialized protobuf `FileDescriptorSet` for the prover service.
#[cfg(feature = "server")]
pub const PROVER_SERVICE_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("prover_service_descriptor");

pub use proto::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    GetProofRequest, GetProofResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse,
    ListProofsRequest, ListProofsResponse, ProofJob, ProofJobStatus, ProofRequest, ProofResult,
    ProofStatus, ProofSummary, ProofType, SnarkGroth16ProofRequest, SnarkGroth16ProofResult,
    SubmitProofRequest, SubmitProofResponse, TeeKind, TeeProofRequest, TeeProofResult, TeeProposal,
    ZkProofRequest, ZkProofResult, ZkVm, proof_request, proof_result, prover_service_client,
};
