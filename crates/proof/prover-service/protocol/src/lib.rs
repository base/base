#![doc = include_str!("../README.md")]

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod api;
#[cfg(feature = "rpc-client")]
pub use api::{ProverRequesterApiClient, ProverWorkerApiClient};
#[cfg(feature = "rpc-server")]
pub use api::{ProverRequesterApiServer, ProverWorkerApiServer};

mod session;
pub use session::ProofSessionId;

mod types;
pub use types::{
    GetNextProofRequest, GetNextProofResponse, GetProofRequest, GetProofResponse, HeartbeatRequest,
    HeartbeatResponse, ListProofsRequest, ListProofsResponse, PROOF_REQUEST_NOT_FOUND_MESSAGE,
    ProofJob, ProofJobStatus, ProofRequest, ProofRequestKind, ProofResult, ProofStatus,
    ProofSummary, ProofType, ProveBlockRangeRequest, ProveBlockRangeResponse,
    SnarkGroth16ProofRequest, SnarkGroth16ProofResult, TeeKind, TeeProofRequest, TeeProofResult,
    WorkerSubmitProofRequest, WorkerSubmitProofResponse, ZkProofRequest, ZkProofResult, ZkVm,
};
