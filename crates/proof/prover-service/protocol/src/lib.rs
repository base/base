#![doc = include_str!("../README.md")]

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod api;
#[cfg(feature = "rpc-client")]
pub use api::{ProverRequesterApiClient, ProverWorkerApiClient};
#[cfg(feature = "rpc-server")]
pub use api::{ProverRequesterApiServer, ProverWorkerApiServer};

mod types;
pub use types::{
    ClaimProofJobRequest, ClaimProofJobResponse, CompleteProofJobRequest, CompleteProofJobResponse,
    FailProofJobRequest, FailProofJobResponse, GetProofJobRequest, GetProofJobResponse,
    GetProofRequest, GetProofResponse, HeartbeatProofJobRequest, HeartbeatProofJobResponse,
    ListProofsRequest, ListProofsResponse, ProofJob, ProofJobStatus, ProofRequest,
    ProofRequestKind, ProofResult, ProofStatus, ProofSummary, ProofType, SnarkGroth16ProofRequest,
    SnarkGroth16ProofResult, SubmitProofRequest, SubmitProofResponse, TeeKind, TeeProofRequest,
    TeeProofResult, TeeProposal, ZkProofRequest, ZkProofResult, ZkVm,
};
