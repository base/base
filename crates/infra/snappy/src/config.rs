//! CLI configuration for the snapshotter sidecar.

use std::path::PathBuf;

use clap::Parser;

/// Configuration for the snapshotter sidecar.
///
/// R2/S3 credentials are read from standard AWS environment variables:
/// - `AWS_ACCESS_KEY_ID`
/// - `AWS_SECRET_ACCESS_KEY`
/// - `AWS_ENDPOINT_URL` (required for R2)
/// - `AWS_REGION` (defaults to `auto` for R2)
#[derive(Debug, Parser)]
#[command(
    name = "base-snappy",
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

    /// S3 endpoint URL (for R2 or `MinIO`). Can also be set via `AWS_ENDPOINT_URL`.
    #[arg(long, env = "AWS_ENDPOINT_URL")]
    pub endpoint_url: Option<String>,

    /// AWS region. Defaults to `auto` for R2.
    #[arg(long, env = "AWS_REGION", default_value = "auto")]
    pub region: String,
}
