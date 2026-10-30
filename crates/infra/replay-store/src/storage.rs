use std::{io::Read, ops::RangeInclusive, path::PathBuf};

use base_replay::{ReplayRecord, ReplayRecordError};
use chrono::{DateTime, Datelike, Utc};
use futures::{Stream, StreamExt, TryStreamExt, stream};
use tokio::io::AsyncReadExt;

use crate::LocalObject;

/// Failures at the storage boundary.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Invalid key, timestamp, range, late record, or oversized object.
    #[error("invalid replay storage input: {0}")]
    Invalid(&'static str),
    /// Local I/O or zstd failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Invalid record JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Invalid JSONL framing.
    #[error(transparent)]
    Record(#[from] ReplayRecordError),
    /// Blocking worker failed.
    #[error(transparent)]
    Worker(#[from] tokio::task::JoinError),
}

/// Local directory containing a `replay-record/v1` dataset.
/// The local directory is recorder-owned; symlinks below its root are rejected.
#[derive(Debug, Clone)]
pub enum ReplayStore {
    /// Root directory; objects are atomically published without replacement.
    Local(PathBuf),
}

impl ReplayStore {
    /// Maximum compressed or uncompressed object size, 64 `MiB`.
    pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
    /// Milliseconds per UTC minute.
    pub const MINUTE_MS: u64 = 60_000;

    /// Format a safe network component and UTC timestamp as a minute object key.
    pub fn key(network: &str, ms: u64) -> Result<String, StoreError> {
        if network.is_empty()
            || !network.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StoreError::Invalid("network component"));
        }
        let time = i64::try_from(ms)
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .filter(|t| t.year() <= 9999)
            .ok_or(StoreError::Invalid("timestamp"))?;
        Ok(format!("{network}/{}.jsonl.zst", time.format("%Y/%m/%d/%H/%M")))
    }

    /// Enumerate inclusive minute keys, skip missing objects, and stream sorted records.
    /// Only one bounded object is decoded at a time; equal timestamps retain arrival order.
    pub fn read<'a>(
        &'a self,
        network: &'a str,
        range: RangeInclusive<u64>,
    ) -> impl Stream<Item = Result<ReplayRecord, StoreError>> + 'a {
        stream::iter((*range.start() / Self::MINUTE_MS)..=(*range.end() / Self::MINUTE_MS))
            .then(move |minute| {
                let range = range.clone();
                async move {
                    let key = Self::key(network, minute * Self::MINUTE_MS)?;
                    let bytes = self.get(&key).await?;
                    let records = tokio::task::spawn_blocking(move || {
                        let mut text = String::new();
                        if let Some(bytes) = bytes {
                            Read::take(
                                zstd::Decoder::new(bytes.as_slice())?,
                                u64::try_from(Self::MAX_OBJECT_BYTES + 1)
                                    .map_err(std::io::Error::other)?,
                            )
                            .read_to_string(&mut text)?;
                        }
                        if text.len() > Self::MAX_OBJECT_BYTES {
                            return Err(StoreError::Invalid("object size"));
                        }
                        let mut records = text
                            .lines()
                            .map(serde_json::from_str::<ReplayRecord>)
                            .collect::<Result<Vec<_>, _>>()?;
                        if records.iter().any(|r| r.received_at_ms / Self::MINUTE_MS != minute) {
                            return Err(StoreError::Invalid("record minute"));
                        }
                        records.retain(|r| range.contains(&r.received_at_ms));
                        records.sort_by_key(|r| r.received_at_ms);
                        Ok::<_, StoreError>(records)
                    })
                    .await??;
                    Ok::<_, StoreError>(stream::iter(records.into_iter().map(Ok)))
                }
            })
            .try_flatten()
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let reader = match self {
            Self::Local(root) => {
                let root = root.clone();
                let key = key.to_owned();
                let opened = tokio::task::spawn_blocking(move || {
                    LocalObject::open(&root, &key, false)?.read()
                })
                .await?;
                match opened {
                    Ok(file) => tokio::fs::File::from_std(file),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
        };
        let mut bytes = Vec::new();
        reader
            .take(u64::try_from(Self::MAX_OBJECT_BYTES + 1).map_err(std::io::Error::other)?)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > Self::MAX_OBJECT_BYTES {
            return Err(StoreError::Invalid("object size"));
        }
        Ok(Some(bytes))
    }

    /// Publish a complete object without replacing an existing key.
    pub async fn put(&self, key: String, bytes: Vec<u8>) -> Result<(), StoreError> {
        if key.split('/').any(|part| part.is_empty() || part == "." || part == "..")
            || bytes.len() > Self::MAX_OBJECT_BYTES
        {
            return Err(StoreError::Invalid("object key or size"));
        }
        match self {
            Self::Local(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || {
                    LocalObject::open(&root, &key, true)?.publish(&bytes)
                })
                .await??;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_replay::ReplayRecord;
    use futures::TryStreamExt;

    use super::ReplayStore;
    use crate::ReplayWriter;

    #[test]
    fn keys_follow_utc_boundaries() {
        for (ms, suffix) in [
            (59_999, "1970/01/01/00/00"),
            (60_000, "1970/01/01/00/01"),
            (3_600_000, "1970/01/01/01/00"),
            (86_400_000, "1970/01/02/00/00"),
            (2_678_400_000, "1970/02/01/00/00"),
            (31_536_000_000, "1971/01/01/00/00"),
        ] {
            assert_eq!(ReplayStore::key("base", ms).unwrap(), format!("base/{suffix}.jsonl.zst"));
        }
        assert!(ReplayStore::key("../escape", 0).is_err());
        assert!(ReplayStore::key("base", u64::MAX).is_err());
    }

    #[tokio::test]
    async fn local_round_trip_ranges_and_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().to_owned());
        let mut writer = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        let params = r#"[ { "n": 1.2300e+02, "s": "\u0061" } ]"#;
        for ms in [59_999, 1, 60_000, 180_000] {
            let record: ReplayRecord = serde_json::from_str(&format!(r#"{{"stream":"builder_ingress","received_at_ms":{ms},"method":"call","params":{params},"source":"[::1]:1"}}"#)).unwrap();
            writer.push(&record).await.unwrap();
        }
        let late: ReplayRecord = serde_json::from_str(r#"{"stream":"builder_ingress","received_at_ms":1,"method":"call","params":[],"source":"x"}"#).unwrap();
        assert!(writer.push(&late).await.is_err());
        writer.close().await.unwrap();
        let records: Vec<_> = store.read("base", 59_999..=180_000).try_collect().await.unwrap();
        assert_eq!(
            records.iter().map(|r| r.received_at_ms).collect::<Vec<_>>(),
            [59_999, 60_000, 180_000]
        );
        assert!(records.iter().all(|r| r.params.get().as_bytes() == params.as_bytes()));
        let mut collision = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        collision.push(&late).await.unwrap();
        assert!(collision.close().await.is_err());
        let records: Vec<_> = store.read("base", 0..=59_999).try_collect().await.unwrap();
        assert_eq!(records.iter().map(|r| r.received_at_ms).collect::<Vec<_>>(), [1, 59_999]);
    }

    #[tokio::test]
    async fn reader_rejects_records_in_wrong_minute() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().to_owned());
        let line = r#"{"stream":"builder_ingress","received_at_ms":60000,"method":"call","params":[],"source":"x"}"#;
        store
            .put(
                ReplayStore::key("base", 0).unwrap(),
                zstd::encode_all(line.as_bytes(), 0).unwrap(),
            )
            .await
            .unwrap();
        let result: Result<Vec<_>, _> = store.read("base", 0..=60_000).try_collect().await;
        assert!(matches!(result, Err(super::StoreError::Invalid("record minute"))));
    }
}
