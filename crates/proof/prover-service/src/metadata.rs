//! Shared proof-session metadata keys used by prover-service RPC responses.

/// Metadata key indicating that a session was produced by the OP Succinct dry-run backend.
pub const OP_SUCCINCT_DRY_RUN_METADATA_KEY: &str = "dry_run";

/// Metadata key where OP Succinct dry-run execution stats are stored on proof sessions.
pub const OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY: &str = "execution_stats";
