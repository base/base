#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]

mod succinct;

pub use succinct::{
    ClusterArtifactStore, ClusterProofConfig, ClusterSessionId, ClusterZkProver,
    ClusterZkProverConfig, DRY_RUN_SNARK_PREFIX, DRY_RUN_STARK_PREFIX, DryRunZkProver,
    L1HeadSource, NetworkZkProver, NetworkZkProverConfig, OpSuccinctWitnessProvider,
    SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig, SuccinctRpcConfig,
    SuccinctZkBackendConfig, SuccinctZkProverBuildError, SuccinctZkProverBuilder,
    SuccinctZkProversConfig, WitnessError, WitnessParams, cluster_setup_keys,
    cluster_setup_range_key, cluster_setup_vkeys, get_agg_proof_stdin, get_range_elf_embedded,
    get_sp1_stdin,
};
