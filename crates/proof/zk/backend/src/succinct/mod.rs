//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target.

mod provider;
pub use provider::{L1HeadSource, OpSuccinctWitnessProvider, WitnessError, WitnessParams};

mod builder;
pub use builder::{
    SuccinctRpcConfig, SuccinctZkBackendConfig, SuccinctZkProverBuildError, SuccinctZkProverBuilder,
};

mod cluster;
pub use cluster::{
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, SuccinctClusterBackendConfig,
};

mod network;
pub use network::{NetworkZkProver, NetworkZkProverConfig, SuccinctNetworkBackendConfig};

mod dry_run;
pub use dry_run::{DRY_RUN_PREFIX, DryRunZkProver};

mod mock;
pub use mock::{MOCK_PROOF_BYTES, MOCK_SNARK_PREFIX, MockZkProver};
