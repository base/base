//! Payload worker trait and load-test subprocess worker.

use std::fs::File;
use std::path::PathBuf;

use async_trait::async_trait;
use reqwest::Url;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::{LoadTestPayloadParams, WeightedTx};
use crate::consensus::FakeMempool;
use crate::error::BenchmarkError;
use crate::process::ProcessHandle;

#[async_trait]
pub trait PayloadWorker: Send + Sync {
    async fn start(&self) -> Result<(), BenchmarkError>;
    async fn stop(&self) -> Result<(), BenchmarkError>;
}

#[derive(serde::Serialize)]
struct LoadTestConfig<'a> {
    rpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_watcher_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flashblocks_ws_url: Option<String>,
    duration: &'static str,
    sender_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    funding_amount: Option<&'a str>,
    transactions: &'a [WeightedTx],
}

pub struct LoadTestPayloadWorker {
    bin: PathBuf,
    rpc_proxy_url: Url,
    block_watcher_url: Option<String>,
    flashblocks_ws_url: Option<String>,
    params: LoadTestPayloadParams,
    funder_key: String,
    log_path: Option<PathBuf>,
    pub mempool: FakeMempool,
    handle: Mutex<Option<ProcessHandle>>,
    config_file: Mutex<Option<NamedTempFile>>,
    stdout_reader:
        Mutex<Option<BufReader<tokio::process::ChildStdout>>>,
}

impl LoadTestPayloadWorker {
    pub fn new(
        bin: PathBuf,
        rpc_proxy_url: Url,
        block_watcher_url: Option<String>,
        flashblocks_ws_url: Option<String>,
        params: LoadTestPayloadParams,
        funder_key: String,
        log_path: Option<PathBuf>,
        mempool: FakeMempool,
    ) -> Self {
        Self {
            bin,
            rpc_proxy_url,
            block_watcher_url,
            flashblocks_ws_url,
            params,
            funder_key,
            log_path,
            mempool,
            handle: Mutex::new(None),
            config_file: Mutex::new(None),
            stdout_reader: Mutex::new(None),
        }
    }
}

#[async_trait]
impl PayloadWorker for LoadTestPayloadWorker {
    async fn start(&self) -> Result<(), BenchmarkError> {
        let cfg = LoadTestConfig {
            rpc: self.rpc_proxy_url.to_string(),
            block_watcher_url: self.block_watcher_url.clone(),
            flashblocks_ws_url: self.flashblocks_ws_url.clone(),
            duration: "99999s",
            sender_count: self.params.sender_count,
            funding_amount: self.params.funding_amount.as_deref(),
            transactions: &self.params.transactions,
        };

        let mut tmp = tempfile::Builder::new()
            .prefix("base-load-test-")
            .suffix(".yaml")
            .tempfile()
            .map_err(BenchmarkError::Io)?;

        serde_yaml::to_writer(&mut tmp, &cfg)
            .map_err(|e| BenchmarkError::Config(e.to_string()))?;

        let config_path = tmp.path().to_path_buf();

        let dev_null = File::open("/dev/null").map_err(BenchmarkError::Io)?;
        let stderr_file = match &self.log_path {
            Some(p) => File::create(p).map_err(BenchmarkError::Io)?,
            None => tempfile::tempfile().map_err(BenchmarkError::Io)?,
        };

        let mut handle = ProcessHandle::new(
            self.bin.clone(),
            vec![config_path.to_string_lossy().into_owned()],
            vec![("FUNDER_KEY".into(), self.funder_key.clone())],
            dev_null,
            stderr_file,
        )
        .with_piped_stdout();
        handle.start().await?;

        let stdout = handle
            .take_stdout()
            .ok_or_else(|| BenchmarkError::Client("load-test stdout pipe missing".into()))?;
        *self.stdout_reader.lock().await = Some(BufReader::new(stdout));

        info!(bin = %self.bin.display(), "load-test subprocess started");

        *self.handle.lock().await = Some(handle);
        *self.config_file.lock().await = Some(tmp);

        Ok(())
    }

    async fn stop(&self) -> Result<(), BenchmarkError> {
        if let Some(mut handle) = self.handle.lock().await.take() {
            handle.stop().await?;
            info!("load-test subprocess stopped");
        }
        Ok(())
    }
}

impl LoadTestPayloadWorker {
    pub async fn wait_until_ready(&self) -> Result<(), BenchmarkError> {
        let mut guard = self.stdout_reader.lock().await;
        let reader = guard.as_mut().ok_or_else(|| {
            BenchmarkError::Client("load-test not started".into())
        })?;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.map_err(BenchmarkError::Io)?;
            if n == 0 {
                return Err(BenchmarkError::Client(
                    "load-test exited before signalling ready".into(),
                ));
            }
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                info!(line = %trimmed, "load-test");
            }
            if trimmed == "Accounts funded." {
                info!("load-test setup complete, starting benchmark");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_worker() -> LoadTestPayloadWorker {
        LoadTestPayloadWorker::new(
            PathBuf::from("/usr/bin/true"),
            "http://127.0.0.1:9999".parse().unwrap(),
            None,
            None,
            LoadTestPayloadParams {
                sender_count: 1,
                funding_amount: None,
                transactions: vec![],
            },
            "0xdeadbeef".into(),
            None,
            FakeMempool::new(),
        )
    }

    #[tokio::test]
    async fn mempool_starts_empty() {
        let worker = make_worker();
        assert!(worker.mempool.drain().is_empty());
    }

    #[tokio::test]
    async fn mempool_add_and_drain() {
        use alloy_primitives::Bytes;
        let worker = make_worker();
        worker
            .mempool
            .add_transactions(vec![Bytes::from_static(b"tx1"), Bytes::from_static(b"tx2")]);
        let drained = worker.mempool.drain();
        assert_eq!(drained.len(), 2);
        assert!(worker.mempool.drain().is_empty());
    }
}
