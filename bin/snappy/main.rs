//! Binary entry point for the snapshotter sidecar.

use anyhow::Result;
use base_snappy::{DockerContainerManager, SnapshotUploader, Snapshotter, SnapshotterConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let config = SnapshotterConfig::parse();

    if let Some(threads) = config.snapshot_threads {
        rayon::ThreadPoolBuilder::new().num_threads(threads).build_global().ok();
    }

    let container_manager = DockerContainerManager::new(&config.docker_socket)?;
    // S3 compatible storage client (i.e., R2, MinIO, etc.)
    let storage_client = build_storage_client(&config.region, config.endpoint_url.as_deref()).await;
    let uploader =
        SnapshotUploader::new(storage_client, config.bucket.clone(), config.prefix.clone());

    let snapshotter = Snapshotter::new(container_manager, uploader, config);
    snapshotter.run().await
}

async fn build_storage_client(region: &str, endpoint_url: Option<&str>) -> aws_sdk_s3::Client {
    let region = aws_sdk_s3::config::Region::new(region.to_owned());
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest()).region(region);

    if let Some(endpoint) = endpoint_url {
        loader = loader.endpoint_url(endpoint);
    }

    let sdk_config = loader.load().await;

    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk_config).force_path_style(true).build(),
    )
}
