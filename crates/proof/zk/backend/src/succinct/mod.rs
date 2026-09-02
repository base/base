//! SP1 (Succinct) ZK proving backends.
//!
//! Each backend implements [`base_proof_zk_host::ZkProver`] for a different SP1
//! execution target. SP1 stdin, ELF setup, and cluster clients live under
//! `utils`.

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

mod utils;
pub use utils::{
    Claim, ClusterArtifactStore, ClusterProofConfig, ClusterProofHandle, ClusterProofHandleJson,
    DisputeGameFactory, GameStatus, GameType, Hash, IDisputeGame, IInitializable,
    OPSuccinctL2OutputOracle, SP1Blobstream, Timestamp, aggregate_vkey, cluster_agg_proof,
    cluster_poll_proof, cluster_range_proof, cluster_setup_keys, cluster_setup_range_key,
    cluster_setup_vkeys, cluster_submit_agg_proof, cluster_submit_range_proof, get_agg_proof_stdin,
    get_cache_dir, get_range_elf_embedded, get_sp1_stdin, get_stdin_cache_path, initialize_host,
    is_cluster_mode, load_stdin_from_cache, range_vkey_commitment, reconstruct_proof_request,
    save_stdin_to_cache, zk_artifact_hash,
};
