#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]

mod succinct;

pub use succinct::{
    Claim, ClusterArtifactStore, ClusterProofConfig, ClusterProofHandle, ClusterProofHandleJson,
    ClusterSessionId, ClusterZkProver, ClusterZkProverConfig, DRY_RUN_SNARK_PREFIX,
    DRY_RUN_STARK_PREFIX, DisputeGameFactory, DryRunZkProver, GameStatus, GameType, Hash,
    IDisputeGame, IInitializable, L1HeadSource, NetworkZkProver, NetworkZkProverConfig,
    OPSuccinctL2OutputOracle, OpSuccinctWitnessProvider, SP1Blobstream,
    SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig, SuccinctRpcConfig,
    SuccinctZkBackendConfig, SuccinctZkProverBuildError, SuccinctZkProverBuilder,
    SuccinctZkProversConfig, Timestamp, WitnessError, WitnessParams, aggregate_vkey,
    cluster_agg_proof, cluster_poll_proof, cluster_range_proof, cluster_setup_keys,
    cluster_setup_range_key, cluster_setup_vkeys, cluster_submit_agg_proof,
    cluster_submit_range_proof, get_agg_proof_stdin, get_cache_dir, get_range_elf_embedded,
    get_sp1_stdin, get_stdin_cache_path, initialize_host, is_cluster_mode, load_stdin_from_cache,
    range_vkey_commitment, reconstruct_proof_request, save_stdin_to_cache, zk_artifact_hash,
};
