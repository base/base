use serde::{Deserialize, Serialize};

/// Versioned export descriptor. All time fields are Unix milliseconds.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportManifest {
    /// Manifest schema identifier.
    pub format: String,
    /// JSONL record schema identifier.
    pub record_format: String,
    /// Storage network namespace.
    pub network: String,
    /// RPC chain ID.
    pub chain_id: u64,
    /// Inclusive first block.
    pub from_block: u64,
    /// Inclusive last block.
    pub to_block: u64,
    /// First canonical block timestamp.
    pub from_timestamp_ms: u64,
    /// Last canonical block timestamp.
    pub to_timestamp_ms: u64,
    /// Requested warmup duration in milliseconds.
    pub warmup_ms: u64,
    /// Inclusive record-window start.
    pub start_ms: u64,
    /// Exclusive record-window end.
    pub end_ms: u64,
    /// Number of JSONL records.
    pub record_count: u64,
}

/// Export input, RPC, storage, or output failure.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Invalid input or timestamp arithmetic.
    #[error("invalid replay export: {0}")]
    Invalid(&'static str),
    /// Requested block is unavailable.
    #[error("canonical block {0} not found")]
    MissingBlock(u64),
    /// RPC discovery failed.
    #[error(transparent)]
    Rpc(#[from] alloy_transport::TransportError),
    /// RPC discovery timed out.
    #[error(transparent)]
    Timeout(#[from] tokio::time::error::Elapsed),
    /// Store read failed.
    #[error(transparent)]
    Store(#[from] base_replay_store::StoreError),
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

#[cfg(test)]
mod tests {
    use std::{ops::RangeInclusive, time::Duration};

    use alloy_rpc_types_eth::Block;
    use base_replay::ReplayRecord;
    use base_replay_store::{ReplayStore, ReplayWriter};
    use clap::Parser;
    use jsonrpsee::{RpcModule, server::ServerBuilder};

    use crate::{ExportCommand, ExportManifest};

    #[rstest::rstest]
    #[case(10..=11, 8_000, Some((52_000, 182_000, vec![52_000, 59_999, 60_000, 60_001, 181_999])))]
    #[case(10..=10, 0, Some((60_000, 62_000, vec![60_000, 60_001])))]
    #[case(10..=10, 61_000, Some((0, 62_000, vec![51_999, 52_000, 59_999, 60_000, 60_001])))]
    #[case(9..=9, 8_000, Some((0, 2_000, vec![])))]
    #[case(RangeInclusive::new(11, 10), 8_000, None)]
    #[case(10..=12, 8_000, None)]
    #[tokio::test]
    async fn exports_window_when_reading_canonical_headers(
        #[case] range: RangeInclusive<u64>,
        #[case] warmup_ms: u64,
        #[case] expected: Option<(u64, u64, Vec<u64>)>,
    ) {
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let mut rpc = RpcModule::new(());
        rpc.register_method("eth_chainId", |_, _, _| "0x2105").unwrap();
        rpc.register_method("eth_getBlockByNumber", |params, _, _| {
            let (number, _): (String, bool) = params.parse().unwrap();
            let (number, timestamp) = match number.as_str() {
                "0x9" => (9, 0),
                "0xa" => (10, 60),
                "0xb" => (11, 180),
                "0xc" => return Ok(None),
                _ => panic!("unexpected block request: {number}"),
            };
            let mut block: Block = Block::default();
            block.header.number = number;
            block.header.timestamp = timestamp;
            Ok::<_, jsonrpsee::types::ErrorObjectOwned>(Some(block))
        })
        .unwrap();
        let handle = server.start(rpc);
        let dir = tempfile::tempdir().unwrap();
        let store = ReplayStore::Local(dir.path().join("store"));
        let mut writer = ReplayWriter::new(store.clone(), "base".into()).unwrap();
        for ms in [51_999, 59_999, 52_000, 60_001, 60_000, 181_999, 182_000] {
            let record: ReplayRecord = serde_json::from_str(&format!(
                r#"{{"stream":"builder_ingress","received_at_ms":{ms},"method":"call","params":[],"source":"test"}}"#
            )).unwrap();
            writer.push(&record).await.unwrap();
        }
        writer.close().await.unwrap();
        let out = dir.path().join("export");
        let mut command = ExportCommand::parse_from([
            "export",
            "--from",
            &range.start().to_string(),
            "--to",
            &range.end().to_string(),
            "--rpc-url",
            &url,
            "--network",
            "base",
            "--store",
            dir.path().join("store").to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);
        assert_eq!(command.warmup, Duration::from_secs(8));
        command.warmup = Duration::from_millis(warmup_ms);

        let result = command.clone().execute().await;

        if let Some((start, end, expected)) = expected {
            let manifest = result.unwrap();
            let records = std::fs::read_to_string(out.join("records.jsonl")).unwrap();
            let timestamps: Vec<_> = records
                .lines()
                .map(|line| serde_json::from_str::<ReplayRecord>(line).unwrap().received_at_ms)
                .collect();
            assert_eq!(timestamps, expected);
            assert_eq!(
                (manifest.start_ms, manifest.end_ms, manifest.record_count),
                (start, end, u64::try_from(expected.len()).unwrap())
            );
            let saved: ExportManifest =
                serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
            assert_eq!(saved, manifest);
            assert!(command.execute().await.is_err());
            assert_eq!(
                (saved.from_block, saved.to_block, saved.chain_id),
                (*range.start(), *range.end(), 8453)
            );
        } else {
            assert!(result.is_err());
            assert!(!out.exists());
        }
        handle.stop().unwrap();
    }
}
