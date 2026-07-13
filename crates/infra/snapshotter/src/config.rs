//! CLI configuration for the snapshotter sidecar.

use std::{num::NonZeroUsize, path::PathBuf};

use clap::{Parser, ValueEnum};
use url::Url;

/// Default tip threshold in seconds: how fresh the latest block must be for the
/// EL to be considered "at tip".
pub const DEFAULT_TIP_THRESHOLD_SECS: u64 = 10;

/// How the S3/R2 client is configured.
#[derive(Debug, Clone, ValueEnum)]
pub enum S3ConfigType {
    /// Uses the standard AWS credential chain (IAM roles, env vars, `~/.aws/credentials`).
    Aws,
    /// Explicit endpoint, access key, and secret key via CLI args or env vars.
    Manual,
}

/// Configuration for the snapshotter sidecar.
#[derive(Debug, Parser)]
#[command(
    name = "base-snapshotter",
    about = "Snapshot and upload reth node data to S3-compatible storage"
)]
pub struct SnapshotterConfig {
    /// Docker container name of the execution layer node to stop/start.
    #[arg(long)]
    pub container_name: String,

    /// Docker container name of the consensus layer node to stop/start.
    #[arg(long)]
    pub consensus_container_name: String,

    /// HTTP JSON-RPC URL of the execution layer node.
    ///
    /// Used to verify the EL is at tip (latest block is recent) before pausing
    /// the container for a snapshot.
    #[arg(long, env = "SNAPSHOTTER_EL_RPC_URL")]
    pub el_rpc_url: Url,

    /// Maximum age (in seconds) of the latest block for the EL to be considered
    /// "at tip".
    ///
    /// If the latest block's timestamp is older than this many seconds relative
    /// to the current wall-clock time, the snapshot run is skipped and the EL
    /// and CL containers are left untouched.
    #[arg(long, env = "SNAPSHOTTER_TIP_THRESHOLD_SECS", default_value_t = DEFAULT_TIP_THRESHOLD_SECS)]
    pub tip_threshold_secs: u64,

    /// Source datadir containing the reth node data (static files + DB).
    #[arg(long, short = 'd')]
    pub source_datadir: PathBuf,

    /// Output directory for snapshot archives and manifest.
    ///
    /// A unique subdirectory is created per run.
    #[arg(long, short = 'o')]
    pub output_dir: PathBuf,

    /// Upload an already-generated `run-<timestamp>` directory from `output_dir`
    /// instead of stopping the EL and regenerating snapshot artifacts.
    #[arg(long)]
    pub upload_existing_run_timestamp: Option<u64>,

    /// S3-compatible bucket name.
    #[arg(long)]
    pub bucket: String,

    /// Key prefix within the bucket (e.g. `mainnet` or `sepolia`).
    #[arg(long, default_value = "")]
    pub prefix: String,

    /// Public HTTP base URL used by download clients.
    ///
    /// This should be the externally reachable base for the snapshot bucket, without
    /// a trailing slash, for example `https://zeronet-v2-snapshots.base.org`.
    #[arg(long, env = "SNAPSHOTTER_PUBLIC_BASE_URL")]
    pub public_base_url: Option<String>,

    /// Chain ID for the snapshot manifest.
    #[arg(long, default_value = "8453")]
    pub chain_id: u64,

    /// Block number for the snapshot. Auto-inferred from the DB if omitted.
    #[arg(long)]
    pub block: Option<u64>,

    /// Blocks per archive file. Auto-inferred from header static files if omitted.
    #[arg(long)]
    pub blocks_per_file: Option<u64>,

    /// Maximum number of threads for snapshot archive creation.
    ///
    /// Defaults to half the available CPUs.
    #[arg(long)]
    pub snapshot_threads: Option<usize>,

    /// Number of completed timestamped snapshot run directories to retain remotely.
    ///
    /// Older `{prefix}/{timestamp}/` directories are deleted after a successful
    /// upload. The append-only `{prefix}/static_files/` directory is never pruned.
    #[arg(long, env = "SNAPSHOTTER_RETAIN_RUNS", default_value = "3")]
    pub retain_runs: NonZeroUsize,

    /// Docker socket path.
    #[arg(long, default_value = "/var/run/docker.sock")]
    pub docker_socket: String,

    /// S3 client configuration mode.
    #[arg(long, env = "SNAPSHOTTER_S3_CONFIG_TYPE", default_value = "aws")]
    pub s3_config_type: S3ConfigType,

    /// S3 endpoint URL (for R2 or `MinIO`). Required for `manual` config type.
    #[arg(long, env = "SNAPSHOTTER_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// S3 region.
    #[arg(long, env = "SNAPSHOTTER_S3_REGION", default_value = "us-east-1")]
    pub s3_region: String,

    /// S3 access key ID. Required for `manual` config type.
    #[arg(long, env = "SNAPSHOTTER_S3_ACCESS_KEY_ID")]
    pub s3_access_key_id: Option<String>,

    /// S3 secret access key. Required for `manual` config type.
    #[arg(long, env = "SNAPSHOTTER_S3_SECRET_ACCESS_KEY")]
    pub s3_secret_access_key: Option<String>,
}
