use std::{path::PathBuf, time::Duration};

use alloy_provider::{Provider, ProviderBuilder};
use aws_config::{BehaviorVersion, Region};
use base_replay::REPLAY_RECORD_FORMAT;
use base_replay_store::{ReplayS3, ReplayStore};
use clap::Parser;
use futures::{TryStreamExt, pin_mut};
use tokio::io::{AsyncWriteExt, BufWriter};
use url::Url;

use crate::{ExportError, ExportManifest};

/// Export recorded ingress for an inclusive canonical block range.
#[derive(Debug, Clone, Parser)]
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
    /// Fixed end-window allowance, independent of subsecond execution slots.
    pub const END_ALLOWANCE_MS: u64 = 2_000;

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
        let provider = ProviderBuilder::new().connect_http(self.rpc_url.clone());
        Ok(self.export(&provider, &store).await?)
    }

    /// Stream to a new directory, publishing the manifest only after flushing records.
    /// RPC discovery is bounded to 30s; cancellation may leave an incomplete directory.
    pub async fn export(
        &self,
        provider: &impl Provider,
        store: &ReplayStore,
    ) -> Result<ExportManifest, ExportError> {
        if self.from > self.to || !self.warmup.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(ExportError::Invalid(
                "reversed block range or fractional millisecond warmup",
            ));
        }
        ReplayStore::key(&self.network, 0)?;
        let warmup_ms = u64::try_from(self.warmup.as_millis())
            .map_err(|_| ExportError::Invalid("warmup overflow"))?;
        let (from, to, chain_id) = tokio::time::timeout(Duration::from_secs(30), async {
            let from = provider
                .get_block_by_number(self.from.into())
                .hashes()
                .await?
                .ok_or(ExportError::MissingBlock(self.from))?;
            let to = provider
                .get_block_by_number(self.to.into())
                .hashes()
                .await?
                .ok_or(ExportError::MissingBlock(self.to))?;
            Ok::<_, ExportError>((
                from.header.timestamp,
                to.header.timestamp,
                provider.get_chain_id().await?,
            ))
        })
        .await??;
        let from_timestamp_ms =
            from.checked_mul(1000).ok_or(ExportError::Invalid("timestamp overflow"))?;
        let to_timestamp_ms =
            to.checked_mul(1000).ok_or(ExportError::Invalid("timestamp overflow"))?;
        if from_timestamp_ms > to_timestamp_ms {
            return Err(ExportError::Invalid("decreasing canonical timestamps"));
        }
        let mut manifest = ExportManifest {
            format: "replay-export/v1".into(),
            record_format: REPLAY_RECORD_FORMAT.into(),
            network: self.network.clone(),
            chain_id,
            from_block: self.from,
            to_block: self.to,
            from_timestamp_ms,
            to_timestamp_ms,
            warmup_ms,
            start_ms: from_timestamp_ms.saturating_sub(warmup_ms),
            end_ms: to_timestamp_ms
                .checked_add(Self::END_ALLOWANCE_MS)
                .ok_or(ExportError::Invalid("timestamp overflow"))?,
            record_count: 0,
        };
        ReplayStore::key(&self.network, manifest.end_ms)?;
        tokio::fs::create_dir(&self.out).await?;
        let mut records =
            BufWriter::new(tokio::fs::File::create_new(self.out.join("records.jsonl")).await?);
        let stream = store.read(&self.network, manifest.start_ms..=manifest.end_ms - 1);
        pin_mut!(stream);
        while let Some(record) = stream.try_next().await? {
            records.write_all(record.to_jsonl_line()?.as_bytes()).await?;
            manifest.record_count += 1;
        }
        records.flush().await?;
        let mut output = tokio::fs::File::create_new(self.out.join("manifest.json.tmp")).await?;
        output.write_all(&serde_json::to_vec_pretty(&manifest)?).await?;
        output.flush().await?;
        tokio::fs::rename(self.out.join("manifest.json.tmp"), self.out.join("manifest.json"))
            .await?;
        Ok(manifest)
    }
}
