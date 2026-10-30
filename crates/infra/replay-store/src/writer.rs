use base_replay::ReplayRecord;

use crate::{ReplayStore, StoreError};

/// Single-network writer. Call `close` explicitly; dropping it discards buffered records.
#[derive(Debug)]
pub struct ReplayWriter {
    store: ReplayStore,
    network: String,
    minute: Option<u64>,
    buffer: Vec<u8>,
}

impl ReplayWriter {
    /// Create a writer with a validated network key component.
    pub fn new(store: ReplayStore, network: String) -> Result<Self, StoreError> {
        ReplayStore::key(&network, 0)?;
        Ok(Self { store, network, minute: None, buffer: Vec::new() })
    }

    /// Buffer a record, publishing the previous minute on forward rollover.
    /// Reject older minutes and oversized objects without accepting the record.
    /// Cancellation during publication can leave an object committed; conflicts are terminal.
    pub async fn push(&mut self, record: &ReplayRecord) -> Result<(), StoreError> {
        let minute = record.received_at_ms / ReplayStore::MINUTE_MS;
        ReplayStore::key(&self.network, record.received_at_ms)?;
        let line = record.to_jsonl_line()?;
        if line.len() > ReplayStore::MAX_OBJECT_BYTES {
            return Err(StoreError::Invalid("object size"));
        }
        if self.minute.is_some_and(|m| minute < m) {
            return Err(StoreError::Invalid("closed minute"));
        }
        if self.minute.is_some_and(|m| minute > m) {
            self.flush().await?;
        }
        if self.buffer.len() + line.len() > ReplayStore::MAX_OBJECT_BYTES {
            return Err(StoreError::Invalid("object size"));
        }
        self.minute = Some(minute);
        self.buffer.extend_from_slice(line.as_bytes());
        Ok(())
    }

    /// Publish the final minute. No object is overwritten, including after restart.
    pub async fn close(mut self) -> Result<(), StoreError> {
        self.flush().await
    }

    async fn flush(&mut self) -> Result<(), StoreError> {
        if let Some(minute) = self.minute {
            let key = ReplayStore::key(&self.network, minute * ReplayStore::MINUTE_MS)?;
            let buffer = self.buffer.clone();
            let bytes = tokio::task::spawn_blocking(move || zstd::encode_all(buffer.as_slice(), 0))
                .await??;
            self.store.put(key, bytes).await?;
            self.buffer.clear();
            self.minute = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_replay::{ReplayRecord, ReplayRecordError};
    use futures::TryStreamExt;

    use super::{ReplayStore, ReplayWriter, StoreError};

    #[tokio::test]
    async fn multiline_record_is_rejected_before_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().to_owned());
        let mut writer = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        let record: ReplayRecord = serde_json::from_str(r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"x"}"#).unwrap();
        writer.push(&record).await.unwrap();
        let invalid: ReplayRecord = serde_json::from_str("{\"stream\":\"builder_ingress\",\"received_at_ms\":60000,\"method\":\"call\",\"params\":[\n1],\"source\":\"x\"}").unwrap();
        assert!(matches!(
            writer.push(&invalid).await,
            Err(StoreError::Record(ReplayRecordError::MultilineParams))
        ));
        let unpublished: Vec<_> = store.read("base", 0..=60_000).try_collect().await.unwrap();
        assert!(unpublished.is_empty());
        writer.close().await.unwrap();
        let records: Vec<_> = store.read("base", 0..=60_000).try_collect().await.unwrap();
        assert_eq!(records.len(), 1);
    }

    #[tokio::test]
    async fn oversized_record_does_not_close_active_minute() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().to_owned());
        let mut writer = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        let mut record: ReplayRecord = serde_json::from_str(r#"{"stream":"builder_ingress","received_at_ms":0,"method":"call","params":[],"source":"x"}"#).unwrap();
        writer.push(&record).await.unwrap();
        record.received_at_ms = 60_000;
        record.method = "x".repeat(ReplayStore::MAX_OBJECT_BYTES);
        assert!(matches!(writer.push(&record).await, Err(StoreError::Invalid("object size"))));
        let unpublished: Vec<_> = store.read("base", 0..=60_000).try_collect().await.unwrap();
        assert!(unpublished.is_empty());
        writer.close().await.unwrap();
        let records: Vec<_> = store.read("base", 0..=60_000).try_collect().await.unwrap();
        assert_eq!(records.len(), 1);
    }
}
