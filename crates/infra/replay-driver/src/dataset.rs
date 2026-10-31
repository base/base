use std::path::Path;

use base_replay::{ReplayRecord, ReplayStream};
use base_replay_export::{ExportManifest, ExportWindow};
use eyre::{Result, WrapErr, ensure};
use tokio::io::{AsyncBufReadExt, BufReader};

/// A validated builder-ingress export. Records remain in capture order until scheduled.
#[derive(Debug)]
pub struct ReplayDataset {
    /// Canonical range and recording metadata.
    pub manifest: ExportManifest,
    /// Captured calls in the manifest's half-open time window.
    pub records: Vec<ReplayRecord>,
}

impl ReplayDataset {
    /// Startup allowance before the earliest exported timestamp.
    pub const CLOCK_MARGIN_MS: u64 = 30_000;

    /// Load the completed export, rejecting unsupported schemas before decoding records.
    pub async fn load(directory: &Path) -> Result<Self> {
        let manifest: ExportManifest =
            serde_json::from_slice(&tokio::fs::read(directory.join("manifest.json")).await?)?;
        ensure!(manifest.format == "replay-export/v1", "unsupported export format");
        ensure!(
            ReplayRecord::supports_format(&manifest.record_format),
            "unsupported record format"
        );
        let mut lines =
            BufReader::new(tokio::fs::File::open(directory.join("records.jsonl")).await?).lines();
        let mut records = Vec::new();
        while let Some(line) = lines.next_line().await? {
            records.push(
                ReplayRecord::from_jsonl_line(&line).wrap_err("invalid replay JSONL record")?,
            );
        }
        Self::new(manifest, records)
    }

    /// Validate an export before making any calls to the builder.
    pub fn new(manifest: ExportManifest, records: Vec<ReplayRecord>) -> Result<Self> {
        ensure!(manifest.format == "replay-export/v1", "unsupported export format");
        ensure!(
            ReplayRecord::supports_format(&manifest.record_format),
            "unsupported record format"
        );
        ensure!(manifest.record_count == u64::try_from(records.len())?, "record count mismatch");
        let window = &manifest.window;
        ensure!(
            window.from_block > 0 && window.from_block <= window.to_block,
            "invalid block range"
        );
        ensure!(
            window.from_timestamp_ms <= window.to_timestamp_ms
                && window.from_timestamp_ms.is_multiple_of(1000)
                && window.to_timestamp_ms.is_multiple_of(1000)
                && window.start_ms == window.from_timestamp_ms.saturating_sub(window.warmup_ms)
                && window.to_timestamp_ms.checked_add(ExportWindow::END_ALLOWANCE_MS)
                    == Some(window.end_ms),
            "invalid export time window"
        );
        for record in &records {
            ensure!(record.stream == ReplayStream::BuilderIngress, "unsupported replay stream");
            ensure!(
                (window.start_ms..window.end_ms).contains(&record.received_at_ms),
                "record outside export window"
            );
        }
        Ok(Self { manifest, records })
    }

    /// Suggested builder epoch. Start the builder before replay, accounting for startup elapsed time.
    pub const fn clock_start_ms(&self) -> u64 {
        self.manifest.window.start_ms.saturating_sub(Self::CLOCK_MARGIN_MS)
    }
}

#[cfg(test)]
mod tests {
    use base_replay::{REPLAY_RECORD_FORMAT, ReplayRecord};
    use base_replay_export::{ExportManifest, ExportWindow, ExportWriter};
    use base_replay_store::{ReplayStore, ReplayWriter};

    use super::ReplayDataset;

    fn manifest() -> ExportManifest {
        ExportManifest {
            format: "replay-export/v1".into(),
            record_format: REPLAY_RECORD_FORMAT.into(),
            network: "base".into(),
            record_count: 0,
            window: ExportWindow {
                chain_id: 8453,
                from_block: 10,
                to_block: 12,
                from_timestamp_ms: 60_000,
                to_timestamp_ms: 64_000,
                warmup_ms: 8_000,
                start_ms: 52_000,
                end_ms: 66_000,
            },
        }
    }

    #[tokio::test]
    async fn loads_real_export_without_changing_captured_parameters() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReplayStore::Local(dir.path().join("store"));
        let record: ReplayRecord = serde_json::from_str(
            r#"{"stream":"builder_ingress","received_at_ms":52000,"method":"call","params":[ { "amount": 1.2300e+02, "n": -0 } ],"source":"test"}"#,
        )?;
        let mut recorder = ReplayWriter::new(store.clone(), "base".into())?;
        recorder.push(&record).await?;
        recorder.close().await?;
        let out = dir.path().join("export");
        ExportWriter { store, network: "base".into() }.export(manifest().window, &out).await?;

        let dataset = ReplayDataset::load(&out).await?;

        assert_eq!(dataset.manifest.record_format, REPLAY_RECORD_FORMAT);
        assert_eq!(dataset.records.len(), 1);
        assert_eq!(dataset.records[0].method, record.method);
        assert_eq!(dataset.records[0].received_at_ms, record.received_at_ms);
        assert_eq!(dataset.records[0].params.get().as_bytes(), record.params.get().as_bytes());
        Ok(())
    }

    #[tokio::test]
    async fn loads_empty_export_when_manifest_matches_records() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_vec(&manifest()).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join("records.jsonl"), "").await.unwrap();

        let dataset = ReplayDataset::load(dir.path()).await.unwrap();

        assert_eq!(dataset.manifest.window.from_block, 10);
        assert!(dataset.records.is_empty());
        assert_eq!(dataset.clock_start_ms(), 22_000);
    }

    #[test]
    fn rejects_export_when_version_count_window_or_stream_is_invalid() {
        for mutation in 0..6 {
            let mut manifest = manifest();
            match mutation {
                0 => manifest.format = "replay-export/v2".into(),
                1 => manifest.record_format = "replay-record/unsupported".into(),
                2 => manifest.record_count = 1,
                3 => manifest.window.from_block = 0,
                4 => manifest.window.start_ms += 1,
                5 => manifest.window.end_ms = 64_000,
                _ => unreachable!(),
            }

            assert!(ReplayDataset::new(manifest, vec![]).is_err());
        }
        for (stream, timestamp) in
            [("builder_ingress", 51_999), ("builder_ingress", 66_000), ("mempool_ingress", 60_000)]
        {
            let mut manifest = manifest();
            manifest.record_count = 1;
            let record = serde_json::from_str(&format!(
                r#"{{"stream":"{stream}","received_at_ms":{timestamp},"method":"call","params":[],"source":"test"}}"#
            )).unwrap();

            assert!(ReplayDataset::new(manifest, vec![record]).is_err());
        }
    }
}
