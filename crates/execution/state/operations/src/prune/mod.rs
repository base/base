//! Proof storage pruner for removing stale trie data.

mod error;
pub use error::{BaseProofStoragePrunerResult, ProofStoragePrunerError, ProofStoragePrunerOutput};

mod pruner;
pub use pruner::BaseProofStoragePruner;

mod metrics;

mod task;
pub use task::BaseProofStoragePrunerTask;
