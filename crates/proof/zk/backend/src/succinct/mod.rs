//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target.

mod provider;
pub use provider::{L1HeadSource, OpSuccinctWitnessProvider, WitnessError, WitnessParams};

mod builder;
pub use builder::{
    SuccinctRpcConfig, SuccinctZkBackendConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, SuccinctZkProversConfig,
};

mod cluster;
pub use cluster::{
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, SuccinctClusterBackendConfig,
};

mod network;
pub use network::{NetworkZkProver, NetworkZkProverConfig, SuccinctNetworkBackendConfig};

mod dry_run;
pub use dry_run::{DRY_RUN_SNARK_PREFIX, DRY_RUN_STARK_PREFIX, DryRunZkProver};

mod stdin;
pub use stdin::get_sp1_stdin;

mod aggregation;
pub use aggregation::get_agg_proof_stdin;

mod network_helpers;
pub use network_helpers::{
    build_network_prover_from_env, determine_network_mode, get_network_signer,
    parse_fulfillment_strategy,
};

mod cluster_utils;
pub use cluster_utils::{
    ClusterArtifactStore, ClusterProofConfig, ClusterProofHandle, ClusterProofHandleJson,
    cluster_agg_proof, cluster_poll_proof, cluster_range_proof, cluster_setup_keys,
    cluster_setup_range_key, cluster_setup_vkeys, cluster_submit_agg_proof,
    cluster_submit_range_proof, get_range_elf_embedded, initialize_host, is_cluster_mode,
    reconstruct_proof_request,
};

mod contract;
pub use contract::{
    Claim, DisputeGameFactory, GameStatus, GameType, Hash, IDisputeGame, IInitializable,
    OPSuccinctL2OutputOracle, SP1Blobstream, Timestamp,
};

mod stats;
pub use stats::{BlockExecutionStats, ExecutionStats, MarkdownExecutionStats};

mod witness_cache;
pub use witness_cache::{
    get_cache_dir, get_stdin_cache_path, load_stdin_from_cache, save_stdin_to_cache,
};
