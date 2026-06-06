//! Binary entry point for the snapshotter sidecar.

use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use base_snapshotter::{
    DockerContainerManager, S3ConfigType, SnapshotUploader, Snapshotter, SnapshotterConfig,
};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let config = SnapshotterConfig::parse();

    if let Some(threads) = config.snapshot_threads
        && let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global()
    {
        warn!(
            threads,
            error = %e,
            "failed to set global rayon thread pool, --snapshot-threads will be ignored"
        );
    }

    let container_manager = DockerContainerManager::new(&config.docker_socket)?;
    let storage_client = create_s3_client(&config).await?;
    let uploader =
        SnapshotUploader::new(storage_client, config.bucket.clone(), config.prefix.clone());

    let snapshotter = Snapshotter::new(container_manager, uploader, config);
    snapshotter.run().await
}

async fn create_s3_client(config: &SnapshotterConfig) -> Result<S3Client> {
    match config.s3_config_type {
        S3ConfigType::Manual => {
            let region = aws_sdk_s3::config::Region::new(config.s3_region.clone());
            let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(region);

            if let Some(ref endpoint) = config.s3_endpoint {
                loader = loader.endpoint_url(endpoint);
            }

            if let (Some(access_key), Some(secret_key)) =
                (&config.s3_access_key_id, &config.s3_secret_access_key)
            {
                let credentials =
                    Credentials::new(access_key, secret_key, None, None, "snapshotter");
                loader = loader.credentials_provider(credentials);
            }

            let sdk_config = loader.load().await;
            let s3_config = S3ConfigBuilder::from(&sdk_config).force_path_style(true);

            info!("using manual S3 client configuration");
            Ok(S3Client::from_conf(s3_config.build()))
        }
        S3ConfigType::Aws => {
            info!("using AWS default S3 client configuration");
            let sdk_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
            Ok(S3Client::new(&sdk_config))
        }
    }
}
