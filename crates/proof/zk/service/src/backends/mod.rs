//! Proving backends for ZK proof generation.

mod mock;
pub use mock::MockBackend;

mod network;
pub use network::NetworkBackend;

mod op_succinct;
pub use op_succinct::{OpSuccinctBackend, OpSuccinctProvider};

mod traits;
pub use traits::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

mod utils;
pub use utils::L1HeadCalculator;
