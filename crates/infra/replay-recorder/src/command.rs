use std::net::SocketAddr;

use aws_config::{BehaviorVersion, Region};
use base_replay_store::{ReplayS3, ReplayStore, ReplayWriter, StoreError};
use clap::Args;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::Recorder;

/// Record HTTP JSON-RPC calls without executing them.
#[derive(Debug, Args)]
pub struct RecordArgs {
    /// HTTP socket address.
    #[arg(long, default_value = "127.0.0.1:8545")]
    pub listen: SocketAddr,
    /// Network component of storage object keys.
    #[arg(long, value_parser = ["sepolia", "mainnet", "devnet"])]
    pub network: String,
    /// Dedicated local capture directory or s3://bucket/prefix.
    #[arg(long)]
    pub store: String,
    /// S3-compatible endpoint URL; credentials use the standard AWS chain.
    #[arg(long)]
    pub s3_endpoint: Option<String>,
    /// S3 signing region, for example us-east-1 or auto for R2.
    #[arg(long)]
    pub s3_region: Option<String>,
}

impl RecordArgs {
    /// Open storage and run until cooperative shutdown, including the final flush.
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), StoreError> {
        let store = if self.store.starts_with("s3://") {
            let mut config = aws_config::defaults(BehaviorVersion::latest());
            if let Some(endpoint) = self.s3_endpoint {
                config = config.endpoint_url(endpoint);
            }
            if let Some(region) = self.s3_region {
                config = config.region(Region::new(region));
            }
            ReplayStore::S3(ReplayS3::new(&self.store, &config.load().await)?)
        } else {
            if self.s3_endpoint.is_some() || self.s3_region.is_some() {
                return Err(StoreError::Invalid("S3 flags require an s3:// store"));
            }
            ReplayStore::Local(self.store.into())
        };
        let writer = ReplayWriter::new(store, self.network)?;
        let listener = TcpListener::bind(self.listen).await?;
        Recorder::new(writer, shutdown).serve(listener).await
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::RecordArgs;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        args: RecordArgs,
    }

    #[test]
    fn parses_network_and_storage_when_flags_are_valid() {
        for network in ["sepolia", "mainnet", "devnet"] {
            let cli = Cli::try_parse_from(["record", "--network", network, "--store", "capture"])
                .unwrap();
            assert_eq!(cli.args.network, network);
            assert_eq!(cli.args.listen.to_string(), "127.0.0.1:8545");
        }
        assert!(
            Cli::try_parse_from(["record", "--network", "unknown", "--store", "capture"]).is_err()
        );
    }
}
