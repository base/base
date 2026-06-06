//! Proving backends for ZK proof generation.

mod op_succinct;
pub use op_succinct::{
    ClusterBackend as OpSuccinctClusterBackend,
    DRY_RUN_METADATA_KEY as OP_SUCCINCT_DRY_RUN_METADATA_KEY,
    DryRunBackend as OpSuccinctDryRunBackend,
    EXECUTION_STATS_METADATA_KEY as OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY,
    MockBackend as OpSuccinctMockBackend, NetworkBackend as OpSuccinctNetworkBackend,
    OpSuccinctProvider, StoredExecutionStats as OpSuccinctStoredExecutionStats,
    WitnessParams as OpSuccinctWitnessParams,
};

mod traits;
pub use traits::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

mod utils;
pub use utils::L1HeadCalculator;
