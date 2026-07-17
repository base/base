#![doc = include_str!("../README.md")]

mod succinct;
pub use succinct::{
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, DRY_RUN_SNARK_PREFIX,
    DRY_RUN_STARK_PREFIX, DryRunZkProver, L1HeadSource, NetworkZkProver, NetworkZkProverConfig,
    OpSuccinctWitnessProvider, SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig,
    SuccinctRpcConfig, SuccinctZkBackendConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, SuccinctZkProversConfig, WitnessError, WitnessParams,
};
