#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]

mod backends;
pub use backends::{
    ArtifactClientWrapper, ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType,
    L1HeadCalculator, OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY,
    OpSuccinctClusterBackend, OpSuccinctDryRunBackend, OpSuccinctMockBackend,
    OpSuccinctNetworkBackend, OpSuccinctProvider, OpSuccinctStoredExecutionStats,
    OpSuccinctWitnessParams, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
};

pub mod metrics;
pub use metrics::ProverMetrics;

mod proof_request_manager;
pub use proof_request_manager::ProofRequestManager;

mod proxy;
pub use proxy::{ProxyConfig, ProxyConfigs, RateLimitConfig, start_all_proxies};

mod server;
pub use server::ProverServiceServer;

mod snark_e2e;
pub use snark_e2e::SnarkE2e;

mod worker;
pub use worker::{ProverWorker, ProverWorkerPool, StatusPoller};
