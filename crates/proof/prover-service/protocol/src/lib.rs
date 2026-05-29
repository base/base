#![doc = include_str!("../README.md")]

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod api;
#[cfg(feature = "rpc-client")]
pub use api::{ProverRequesterApiClient, ProverWorkerApiClient};
#[cfg(feature = "rpc-server")]
pub use api::{ProverRequesterApiServer, ProverWorkerApiServer};

mod types;
pub use types::{
    GetNextProofRequest, GetNextProofResponse, GetProofRequest, GetProofResponse, HeartbeatRequest,
    HeartbeatResponse, ListProofsRequest, ListProofsResponse, ProofJob, ProofJobStatus,
    ProofRequest, ProofRequestKind, ProofResult, ProofStatus, ProofSummary, ProofType,
    SnarkGroth16ProofRequest, SnarkGroth16ProofResult, SubmitProofRequest, SubmitProofResponse,
    TeeKind, TeeProofRequest, TeeProofResult, TeeProposal, WorkerSubmitProofRequest,
    WorkerSubmitProofResponse, ZkProofRequest, ZkProofResult, ZkVm,
};
