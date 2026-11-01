use std::path::Path;

use base_replay::REPLAY_RECORD_FORMAT;
use base_replay_store::{ReplayStore, StoreError};
use futures::{TryStreamExt, pin_mut};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufWriter};

use crate::ExportWindow;

/// Versioned descriptor for the adjacent `records.jsonl` file.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    /// Manifest schema identifier.
    pub format: String,
    /// JSONL record schema identifier.
    pub record_format: String,
    /// Storage network namespace.
    pub network: String,
    /// Canonical endpoints, warmup, and record window in Unix milliseconds.
    #[serde(flatten)]
    pub window: ExportWindow,
    /// Number of JSONL records.
    pub record_count: u64,
}

/// Storage or export publication failure.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Store read or window validation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Record cannot be represented as JSONL.
    #[error(transparent)]
    Record(#[from] base_replay::ReplayRecordError),
    /// Output I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Manifest serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Source dataset for bounded-memory replay exports.
#[derive(Debug)]
pub struct ExportWriter {
    /// Local or S3 minute storage.
    pub store: ReplayStore,
    /// Storage network namespace.
    pub network: String,
}

impl ExportWriter {
    /// Stream into a new directory, publishing the manifest last.
    /// Errors or cancellation may leave a partial directory without a manifest.
    pub async fn export(
        &self,
        window: ExportWindow,
        out: &Path,
    ) -> Result<ExportManifest, ExportError> {
        let end = window
            .end_ms
            .checked_sub(1)
            .filter(|end| *end >= window.start_ms)
            .ok_or(StoreError::Invalid("time window"))?;
        ReplayStore::key(&self.network, window.start_ms)?;
        ReplayStore::key(&self.network, end)?;
        let mut manifest = ExportManifest {
            format: "replay-export/v1".into(),
            record_format: REPLAY_RECORD_FORMAT.into(),
            network: self.network.clone(),
            window,
            record_count: 0,
        };
        tokio::fs::create_dir(out).await?;
        let mut records =
            BufWriter::new(tokio::fs::File::create_new(out.join("records.jsonl")).await?);
        let stream = self.store.read(&self.network, manifest.window.start_ms..=end);
        pin_mut!(stream);
        while let Some(record) = stream.try_next().await? {
            records.write_all(record.to_jsonl_line()?.as_bytes()).await?;
            manifest.record_count += 1;
        }
        records.flush().await?;
        let mut output = tokio::fs::File::create_new(out.join("manifest.json.tmp")).await?;
        output.write_all(&serde_json::to_vec_pretty(&manifest)?).await?;
        output.flush().await?;
        tokio::fs::rename(out.join("manifest.json.tmp"), out.join("manifest.json")).await?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use base_replay::ReplayRecord;
    use base_replay_store::{ReplayStore, ReplayWriter};

    use super::{ExportManifest, ExportWriter};
    use crate::ExportWindow;

    fn window() -> ExportWindow {
        ExportWindow {
            chain_id: 8453,
            from_block: 10,
            to_block: 11,
            from_timestamp_ms: 60_000,
            to_timestamp_ms: 180_000,
            warmup_ms: 8_000,
            start_ms: 52_000,
            end_ms: 182_000,
        }
    }

    #[rstest::rstest]
    #[case("base", vec![52_000, 59_999, 60_000, 60_001, 181_999])]
    #[case("empty", vec![])]
    #[tokio::test]
    async fn exports_ordered_window_when_minutes_are_missing(
        #[case] network: &str,
        #[case] expected: Vec<u64>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().join("store"));
        let mut recorder = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        for ms in [51_999, 59_999, 52_000, 60_001, 60_000, 181_999, 182_000] {
            let record: ReplayRecord = serde_json::from_str(&format!(
                r#"{{"stream":"builder_ingress","received_at_ms":{ms},"method":"call","params":[],"source":"test"}}"#
            )).unwrap();
            recorder.push(&record).await.unwrap();
        }
        recorder.close().await.unwrap();
        let writer = ExportWriter { store, network: network.into() };
        let out = dir.path().join("export");

        let manifest = writer.export(window(), &out).await.unwrap();

        let records = std::fs::read_to_string(out.join("records.jsonl")).unwrap();
        let timestamps: Vec<_> = records
            .lines()
            .map(|line| ReplayRecord::from_jsonl_line(line).unwrap().received_at_ms)
            .collect();
        assert_eq!(timestamps, expected);
        assert_eq!(manifest.record_count, u64::try_from(expected.len()).unwrap());
        let saved: ExportManifest =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(saved, manifest);
        let json = serde_json::to_value(&saved).unwrap();
        assert_eq!(json["format"], "replay-export/v1");
        assert_eq!(json["record_format"], "replay-record/v2");
        assert_eq!(json["start_ms"], 52_000);
        assert_eq!(json["end_ms"], 182_000);
        assert!(writer.export(window(), &out).await.is_err());
    }

    #[tokio::test]
    async fn leaves_no_manifest_when_store_decode_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().join("store"));
        store.put(ReplayStore::key("base", 52_000).unwrap(), vec![0]).await.unwrap();
        let writer = ExportWriter { store, network: "base".into() };
        let out = dir.path().join("export");

        let result = writer.export(window(), &out).await;

        assert!(result.is_err());
        assert!(!out.join("manifest.json").exists());
    }
}
