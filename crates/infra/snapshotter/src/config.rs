//! CLI configuration for the snapshotter sidecar.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

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

    /// Source datadir containing the reth node data (static files + DB).
    #[arg(long, short = 'd')]
    pub source_datadir: PathBuf,

    /// Output directory for snapshot archives and manifest.
    ///
    /// A unique subdirectory is created per run.
    #[arg(long, short = 'o')]
    pub output_dir: PathBuf,

    /// S3-compatible bucket name.
    #[arg(long)]
    pub bucket: String,

    /// Key prefix within the bucket (e.g. `mainnet` or `sepolia`).
    #[arg(long, default_value = "")]
    pub prefix: String,

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
