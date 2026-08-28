//! SP1 stdin, ELF, and proving-client helpers used by the Succinct backends.

mod stdin;
pub use stdin::{get_agg_proof_stdin, get_sp1_stdin};

mod elf;
pub use elf::{
    aggregate_vkey, cluster_setup_keys, cluster_setup_range_key, cluster_setup_vkeys,
    get_range_elf_embedded, range_vkey_commitment, zk_artifact_hash,
};

mod cluster;
pub use cluster::{
    ClusterArtifactStore, ClusterProofConfig, ClusterProofHandle, ClusterProofHandleJson,
    cluster_agg_proof, cluster_poll_proof, cluster_range_proof, cluster_submit_agg_proof,
    cluster_submit_range_proof, initialize_host, is_cluster_mode, reconstruct_proof_request,
};

mod contract;
pub use contract::{
    Claim, DisputeGameFactory, GameStatus, GameType, Hash, IDisputeGame, IInitializable,
    OPSuccinctL2OutputOracle, SP1Blobstream, Timestamp,
};

mod cache;
pub use cache::{get_cache_dir, get_stdin_cache_path, load_stdin_from_cache, save_stdin_to_cache};
