//! Proving backends for ZK proof generation.

mod op_succinct;
pub use op_succinct::{
    ClusterBackend as OpSuccinctClusterBackend, MockBackend as OpSuccinctMockBackend,
    NetworkBackend as OpSuccinctNetworkBackend, OpSuccinctProvider,
    WitnessParams as OpSuccinctWitnessParams,
};

mod traits;
pub use traits::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

mod utils;
pub use utils::L1HeadCalculator;
