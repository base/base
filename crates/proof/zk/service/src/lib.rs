#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]

mod backends;
pub use backends::{
    ArtifactStorageConfig, BackendConfig, BackendRegistry, BackendType, ProofProcessingResult,
    ProveResult, ProvingBackend, SessionStatus, build_backend,
};

mod proof_request_manager;
pub use proof_request_manager::ProofRequestManager;

mod proxy;
pub use proxy::{ProxyConfig, ProxyConfigs, RateLimitConfig, start_all_proxies};

mod server;
pub use server::ProverServiceServer;

mod worker;
pub use worker::{ProverWorker, ProverWorkerPool, StatusPoller};
