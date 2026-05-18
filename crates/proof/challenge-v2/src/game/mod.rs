//! Game pipeline: discovery, violation detection, proving, and per-game
//! workers that emit dispute submissions for invalid intermediate roots.

mod account_proof;
pub use account_proof::{AccountProofError, AccountProofVerifier};

mod output_validator;
pub use output_validator::{L2OutputValidator, OutputRootError, OutputValidator};

mod discovery;
pub use discovery::{ClassifyError, GameDiscovery, GameInfo, ProvingState};

mod violation;
pub use violation::{ValidationError, Violation, ViolationKind};

mod tee_proof_provider;
pub use tee_proof_provider::{
    RpcTeeProofProvider, TeeProofError, TeeProofProvider, TeeProofResult,
};

mod prove;
pub use prove::ProofError;

mod dispute_action;
pub use dispute_action::{DisputeAction, DisputeRequest};

mod worker;
pub use worker::{GameWorkerConfig, GameWorkerDeps, run_game_worker};

mod pool;
pub use pool::GamePool;
