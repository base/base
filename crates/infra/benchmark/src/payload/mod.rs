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

/// Drives a payload generation subprocess during a benchmark run.
#[async_trait]
pub trait PayloadWorker: Send + Sync {
    /// Launch the payload worker subprocess.
    async fn start(&self) -> Result<(), BenchmarkError>;
    /// Stop the payload worker subprocess.
    async fn stop(&self) -> Result<(), BenchmarkError>;
}

/// Grouped constructor arguments for [`LoadTestPayloadWorker`].
#[derive(Debug)]
pub struct LoadTestConfig {
    /// Path to the `base-load-test` binary.
    pub bin: PathBuf,
    /// RPC proxy URL the load-test sends transactions to.
    pub rpc_proxy_url: Url,
    /// Optional block-watcher URL for pacing.
    pub block_watcher_url: Option<String>,
    /// Optional flashblocks WebSocket URL.
    pub flashblocks_ws_url: Option<String>,
    /// Load-test payload parameters.
    pub params: LoadTestPayloadParams,
    /// Hex-encoded private key for the funder account.
    pub funder_key: String,
    /// Path to write stderr logs to.
    pub log_path: Option<PathBuf>,
    /// Shared mempool for intercepted transactions.
    pub mempool: FakeMempool,
}

#[derive(serde::Serialize)]
struct LoadTestYamlConfig<'a> {
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

/// Spawns and manages a `base-load-test` subprocess that generates
/// transactions and feeds them into a [`FakeMempool`].
pub struct LoadTestPayloadWorker {
    bin: PathBuf,
    rpc_proxy_url: Url,
    block_watcher_url: Option<String>,
    flashblocks_ws_url: Option<String>,
    params: LoadTestPayloadParams,
    funder_key: String,
    log_path: Option<PathBuf>,
    /// Shared pending-transaction pool.
    pub mempool: FakeMempool,
    handle: Mutex<Option<ProcessHandle>>,
    config_file: Mutex<Option<NamedTempFile>>,
    stdout_reader:
        Mutex<Option<BufReader<tokio::process::ChildStdout>>>,
}

impl std::fmt::Debug for LoadTestPayloadWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadTestPayloadWorker")
            .field("bin", &self.bin)
            .finish_non_exhaustive()
    }
}

impl LoadTestPayloadWorker {
    /// Create a new worker from the provided configuration.
    pub fn new(config: LoadTestConfig) -> Self {
        Self {
            bin: config.bin,
            rpc_proxy_url: config.rpc_proxy_url,
            block_watcher_url: config.block_watcher_url,
            flashblocks_ws_url: config.flashblocks_ws_url,
            params: config.params,
            funder_key: config.funder_key,
            log_path: config.log_path,
            mempool: config.mempool,
            handle: Mutex::new(None),
            config_file: Mutex::new(None),
            stdout_reader: Mutex::new(None),
        }
    }
}

#[async_trait]
impl PayloadWorker for LoadTestPayloadWorker {
    async fn start(&self) -> Result<(), BenchmarkError> {
        let cfg = LoadTestYamlConfig {
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
    /// Block until the load-test prints `"Accounts funded."` on stdout.
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
        LoadTestPayloadWorker::new(LoadTestConfig {
            bin: PathBuf::from("/usr/bin/true"),
            rpc_proxy_url: "http://127.0.0.1:9999".parse().unwrap(),
            block_watcher_url: None,
            flashblocks_ws_url: None,
            params: LoadTestPayloadParams {
                sender_count: 1,
                funding_amount: None,
                transactions: vec![],
            },
            funder_key: "0xdeadbeef".into(),
            log_path: None,
            mempool: FakeMempool::new(),
        })
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
