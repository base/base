//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target. SP1 stdin and ELF setup live
//! under `utils`.

mod provider;
pub use provider::{L1HeadSource, OpSuccinctWitnessProvider, WitnessError, WitnessParams};

mod builder;
pub use builder::{
    SuccinctRpcConfig, SuccinctZkBackendConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, SuccinctZkProversConfig,
};

mod cluster;
pub use cluster::{
    ClusterArtifactStore, ClusterProofConfig, ClusterSessionId, ClusterZkProver,
    ClusterZkProverConfig, SuccinctClusterBackendConfig,
};

mod network;
pub use network::{NetworkZkProver, NetworkZkProverConfig, SuccinctNetworkBackendConfig};

mod dry_run;
pub use dry_run::{DRY_RUN_SNARK_PREFIX, DRY_RUN_STARK_PREFIX, DryRunZkProver};

mod utils;
pub use utils::{
    cluster_setup_keys, cluster_setup_range_key, cluster_setup_vkeys, get_agg_proof_stdin,
    get_range_elf_embedded, get_sp1_stdin,
};
