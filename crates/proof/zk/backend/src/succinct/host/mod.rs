//! SP1 host helpers used by the Succinct proving backends.

mod stdin;
pub use stdin::{get_agg_proof_stdin, get_sp1_stdin};

mod elf;
pub use elf::{
    cluster_setup_keys, cluster_setup_range_key, cluster_setup_vkeys, get_range_elf_embedded,
};

mod cluster;
pub use cluster::{
    ClusterArtifactStore, ClusterProofConfig, ClusterProofHandle, ClusterProofHandleJson,
    cluster_agg_proof, cluster_poll_proof, cluster_range_proof, cluster_submit_agg_proof,
    cluster_submit_range_proof, initialize_host, is_cluster_mode, reconstruct_proof_request,
};

mod network;
pub use network::{
    build_network_prover_from_env, determine_network_mode, get_network_signer,
    parse_fulfillment_strategy,
};

mod contract;
pub use contract::{
    Claim, DisputeGameFactory, GameStatus, GameType, Hash, IDisputeGame, IInitializable,
    OPSuccinctL2OutputOracle, SP1Blobstream, Timestamp,
};

mod stats;
pub use stats::{BlockExecutionStats, ExecutionStats, MarkdownExecutionStats};

mod cache;
pub use cache::{get_cache_dir, get_stdin_cache_path, load_stdin_from_cache, save_stdin_to_cache};
