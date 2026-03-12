//! Proving backends for ZK proof generation.

mod generic_zkvm;
mod traits;

pub use generic_zkvm::build_backend;
pub use traits::{
    ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType, ProofProcessingResult,
    ProveResult, ProvingBackend, SessionStatus,
};
