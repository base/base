//! Binary entry point for the snapshotter sidecar.

use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client as S3Client, config::Builder as S3ConfigBuilder};
use base_cli_utils::LogConfig;
use base_snapshotter::{
    DockerContainerManager, RpcTipChecker, S3ConfigType, SnapshotUploader, Snapshotter,
    SnapshotterConfig,
};
use clap::Parser;
use tracing::{info, warn};

base_cli_utils::define_log_args!("SNAPSHOTTER");

/// Snapshotter command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "base-snapshotter",
    about = "Snapshot and upload reth node data to S3-compatible storage"
)]
struct Cli {
    /// Logging configuration.
    #[command(flatten)]
    logging: LogArgs,

    /// Snapshot configuration.
    #[command(flatten)]
    snapshotter: SnapshotterConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    LogConfig::from(cli.logging)
        .init_tracing_subscriber()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing: {error}"))?;
    let config = cli.snapshotter;

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
    let tip_checker = RpcTipChecker::new(config.el_rpc_url.clone());
    let storage_client = create_s3_client(&config).await?;
    let uploader = SnapshotUploader::new(
        storage_client,
        config.bucket.clone(),
        config.prefix.clone(),
        config.public_base_url.clone(),
    );

    let snapshotter = Snapshotter::new(container_manager, tip_checker, uploader, config);
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use base_cli_utils::LogFormat;
    use clap::{CommandFactory, Parser};

    use super::Cli;

    #[test]
    fn supports_shared_log_format_configuration() {
        let cli = Cli::try_parse_from([
            "base-snapshotter",
            "--logs.stdout.format=json",
            "--container-name=execution",
            "--consensus-container-name=consensus",
            "--el-rpc-url=http://execution:8545",
            "--source-datadir=/data",
            "--output-dir=/snapshots",
            "--bucket=snapshots",
        ])
        .unwrap();

        assert_eq!(cli.logging.stdout_format, LogFormat::Json);

        let command = Cli::command();
        let format = command
            .get_arguments()
            .find(|arg| arg.get_long() == Some("logs.stdout.format"))
            .unwrap();
        assert_eq!(format.get_env(), Some(OsStr::new("SNAPSHOTTER_LOG_FORMAT")));
    }
}
