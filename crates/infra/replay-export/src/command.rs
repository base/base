use std::{path::PathBuf, time::Duration};

use alloy_provider::ProviderBuilder;
use aws_config::{BehaviorVersion, Region};
use base_replay_store::{ReplayS3, ReplayStore};
use clap::Parser;
use url::Url;

use crate::{ExportManifest, ExportWindow, ExportWriter};

/// Export recorded ingress for an inclusive canonical block range.
#[derive(Debug, Parser)]
pub struct ExportCommand {
    /// First canonical L2 block, inclusive.
    #[arg(long)]
    pub from: u64,
    /// Last canonical L2 block, inclusive.
    #[arg(long)]
    pub to: u64,
    /// L2 HTTP RPC endpoint.
    #[arg(long)]
    pub rpc_url: Url,
    /// Storage network namespace.
    #[arg(long)]
    pub network: String,
    /// Local root or `s3://bucket/prefix`.
    #[arg(long)]
    pub store: String,
    /// S3-compatible endpoint override.
    #[arg(long)]
    pub s3_endpoint: Option<String>,
    /// AWS region override, for example auto for R2.
    #[arg(long)]
    pub s3_region: Option<String>,
    /// Duration before the first block timestamp.
    #[arg(long, default_value = "8s", value_parser = humantime::parse_duration)]
    pub warmup: Duration,
    /// New output directory; existing paths are rejected.
    #[arg(long)]
    pub out: PathBuf,
}

impl ExportCommand {
    /// Configure RPC and storage, then export. Errors leave no completion manifest.
    pub async fn execute(self) -> eyre::Result<ExportManifest> {
        eyre::ensure!(matches!(self.rpc_url.scheme(), "http" | "https"), "expected HTTP RPC URL");
        let store = if self.store.starts_with("s3://") {
            let mut config = aws_config::defaults(BehaviorVersion::latest());
            if let Some(endpoint) = &self.s3_endpoint {
                config = config.endpoint_url(endpoint);
            }
            if let Some(region) = &self.s3_region {
                config = config.region(Region::new(region.clone()));
            }
            ReplayStore::S3(ReplayS3::new(&self.store, &config.load().await)?)
        } else {
            eyre::ensure!(
                self.s3_endpoint.is_none() && self.s3_region.is_none(),
                "S3 options require an S3 store"
            );
            ReplayStore::Local(PathBuf::from(&self.store))
        };
        let provider = ProviderBuilder::new().connect_http(self.rpc_url);
        let window = ExportWindow::resolve(&provider, self.from..=self.to, self.warmup).await?;
        Ok(ExportWriter { store, network: self.network }.export(window, &self.out).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use super::ExportCommand;

    #[rstest::rstest]
    #[case(None, 8_000)]
    #[case(Some("1500ms"), 1_500)]
    #[case(Some("0s"), 0)]
    fn parses_warmup_override(#[case] warmup: Option<&str>, #[case] expected_ms: u64) {
        let mut args = vec![
            "export",
            "--from",
            "10",
            "--to",
            "11",
            "--rpc-url",
            "http://localhost:8545",
            "--network",
            "base",
            "--store",
            "./store",
            "--out",
            "./export",
        ];
        if let Some(warmup) = warmup {
            args.extend(["--warmup", warmup]);
        }

        let command = ExportCommand::parse_from(args);

        assert_eq!(command.warmup, Duration::from_millis(expected_ms));
    }
}
