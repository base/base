//! EL client abstraction: trait, options, and concrete implementations.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::error::BenchmarkError;
use crate::flashblocks::FlashblocksClient;
use crate::ports::PortManager;
use crate::process::ProcessHandle;

const RPC_POLL_INTERVAL: Duration = Duration::from_millis(500);
const RPC_READY_TIMEOUT: Duration = Duration::from_secs(240);

/// Public options provided by the benchmark config for a given node.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Node type string: `"base-reth-node"` or `"builder"`.
    pub node_type: String,
    /// Extra CLI arguments appended after standard args.
    pub extra_args: Vec<String>,
    /// Path to the `base-reth-node` binary.
    pub reth_bin: PathBuf,
    /// Path to the `base-builder` binary.
    pub builder_bin: PathBuf,
    /// Flashblocks block time in milliseconds (builder only).
    pub flashblocks_block_time_ms: Option<u64>,
}

/// Resolved internal paths and secrets for a single node instance.
#[derive(Debug, Clone)]
pub struct InternalClientOptions {
    /// Path to the JWT secret file.
    pub jwt_secret_path: PathBuf,
    /// Path to the chain config JSON file.
    pub chain_cfg_path: PathBuf,
    /// Path to the node data directory.
    pub data_dir_path: PathBuf,
    /// Path to the test working directory.
    pub test_dir_path: PathBuf,
    /// Raw JWT secret bytes.
    pub jwt_secret: [u8; 32],
    /// Path to the metrics output directory.
    pub metrics_path: PathBuf,
}

/// Abstracts over EL client implementations (BaseRethNode, Builder).
#[async_trait]
pub trait ExecutionClient: Send + Sync {
    /// Start the client process and wait until its RPC is ready.
    async fn run(&mut self) -> Result<(), BenchmarkError>;

    /// Stop the client process and release all acquired ports.
    async fn stop(&mut self) -> Result<(), BenchmarkError>;

    /// HTTP RPC URL (`http://127.0.0.1:<port>`).
    fn rpc_url(&self) -> &str;

    /// Authenticated Engine API RPC URL (`http://127.0.0.1:<port>`).
    fn auth_rpc_url(&self) -> &str;

    /// Port on which the node exposes its Prometheus metrics.
    fn metrics_port(&self) -> u16;

    /// Run `binary --version` and parse the version string.
    async fn get_version(&self) -> Result<String, BenchmarkError>;

    /// Call `debug_setHead` to rewind the node to a given block number.
    async fn set_head(&self, block: u64) -> Result<(), BenchmarkError>;

    /// Return the flashblocks WS client, if this node produces flashblocks.
    fn flashblocks_client(&self) -> Option<&FlashblocksClient>;

    /// Returns `true` if this node receives flashblocks (i.e. a validator
    /// connected to a replay server).
    fn supports_flashblocks(&self) -> bool;
}

/// Drives a `base-reth-node` process with four ports: EL RPC, Auth RPC,
/// Prometheus metrics, and P2P.
pub struct BaseRethNodeClient {
    options: ClientOptions,
    internal: InternalClientOptions,
    ports: Vec<u16>,
    port_manager: Arc<PortManager>,
    process: Option<ProcessHandle>,
    rpc_url: String,
    auth_rpc_url: String,
    websocket_url: Option<String>,
}

impl BaseRethNodeClient {
    /// Create a new client. Ports are not acquired until [`run`](Self::run).
    pub fn new(
        options: ClientOptions,
        internal: InternalClientOptions,
        port_manager: Arc<PortManager>,
        websocket_url: Option<String>,
    ) -> Self {
        Self {
            options,
            internal,
            ports: vec![],
            port_manager,
            process: None,
            rpc_url: String::new(),
            auth_rpc_url: String::new(),
            websocket_url,
        }
    }

    fn binary(&self) -> &Path {
        &self.options.reth_bin
    }

    fn build_args(&self, el_port: u16, auth_port: u16, metrics_port: u16, p2p_port: u16) -> Vec<String> {
        let mut args = vec![
            "node".into(),
            "--color".into(), "never".into(),
            "--chain".into(), self.internal.chain_cfg_path.to_string_lossy().into_owned(),
            "--datadir".into(), self.internal.data_dir_path.to_string_lossy().into_owned(),
            "--http".into(),
            "--http.port".into(), el_port.to_string(),
            "--http.api".into(), "eth,net,web3,miner,debug".into(),
            "--authrpc.port".into(), auth_port.to_string(),
            "--authrpc.jwtsecret".into(), self.internal.jwt_secret_path.to_string_lossy().into_owned(),
            "--metrics".into(), metrics_port.to_string(),
            "--engine.state-provider-metrics".into(),
            "--disable-discovery".into(),
            "--port".into(), p2p_port.to_string(),
            "-vvv".into(),
            "--txpool.pending-max-count".into(), "100000000".into(),
            "--txpool.queued-max-count".into(), "100000000".into(),
            "--txpool.max-account-slots".into(), "100000000".into(),
            "--txpool.pending-max-size".into(), "100".into(),
            "--txpool.queued-max-size".into(), "100".into(),
            "--db.read-transaction-timeout".into(), "0".into(),
        ];

        for arg in &self.options.extra_args {
            args.push(arg.clone());
        }

        if let Some(ws_url) = &self.websocket_url {
            args.push("--websocket-url".into());
            args.push(ws_url.clone());
        }

        args
    }

    async fn wait_for_rpc(&self, url: &str) -> Result<(), BenchmarkError> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_chainId",
            "params": [],
            "id": 1,
        });

        let deadline = tokio::time::Instant::now() + RPC_READY_TIMEOUT;
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(BenchmarkError::Timeout(format!(
                    "RPC at {url} not ready after {}s",
                    RPC_READY_TIMEOUT.as_secs()
                )));
            }
            match client.post(url).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    info!(url = %url, "RPC ready");
                    return Ok(());
                }
                _ => {
                    tokio::time::sleep(RPC_POLL_INTERVAL).await;
                }
            }
        }
    }
}

#[async_trait]
impl ExecutionClient for BaseRethNodeClient {
    async fn run(&mut self) -> Result<(), BenchmarkError> {
        let ports = self.port_manager.acquire_n(4)?;
        let (el_port, auth_port, metrics_port, p2p_port) =
            (ports[0], ports[1], ports[2], ports[3]);
        self.ports = ports;

        self.rpc_url = format!("http://127.0.0.1:{el_port}");
        self.auth_rpc_url = format!("http://127.0.0.1:{auth_port}");

        let backup = self.internal.data_dir_path.join("txpool-transactions-backup.rlp");
        if backup.exists() {
            if let Err(e) = fs::remove_file(&backup) {
                warn!(path = %backup.display(), error = %e, "failed to remove txpool backup");
            }
        }

        let args = self.build_args(el_port, auth_port, metrics_port, p2p_port);

        let log_path = self.internal.test_dir_path.join("el.log");
        let log_file = File::create(&log_path).map_err(|e| {
            BenchmarkError::Io(e)
        })?;
        let log_file2 = log_file.try_clone()?;

        let mut handle = ProcessHandle::new(
            self.binary().to_path_buf(),
            args,
            vec![],
            log_file,
            log_file2,
        );
        handle.start().await?;
        self.process = Some(handle);

        self.wait_for_rpc(&self.rpc_url.clone()).await?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), BenchmarkError> {
        if let Some(mut p) = self.process.take() {
            p.stop().await?;
        }
        self.port_manager.release_all(&self.ports);
        self.ports.clear();
        Ok(())
    }

    fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    fn auth_rpc_url(&self) -> &str {
        &self.auth_rpc_url
    }

    fn metrics_port(&self) -> u16 {
        self.ports.get(2).copied().unwrap_or(0)
    }

    async fn get_version(&self) -> Result<String, BenchmarkError> {
        let output = tokio::process::Command::new(self.binary())
            .arg("--version")
            .output()
            .await
            .map_err(|e| BenchmarkError::Client(format!("failed to run --version: {e}")))?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Version: ") {
                return Ok(rest.trim().to_string());
            }
        }
        Err(BenchmarkError::Client("could not parse version from --version output".into()))
    }

    async fn set_head(&self, block: u64) -> Result<(), BenchmarkError> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "debug_setHead",
            "params": [format!("0x{block:x}")],
            "id": 1,
        });
        client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BenchmarkError::Client(format!("debug_setHead failed: {e}")))?;
        Ok(())
    }

    fn flashblocks_client(&self) -> Option<&FlashblocksClient> {
        None
    }

    fn supports_flashblocks(&self) -> bool {
        false
    }
}

/// Wraps [`BaseRethNodeClient`] with a flashblocks WebSocket port, producing
/// flashblocks for a connected validator or replay server.
pub struct BuilderClient {
    inner: BaseRethNodeClient,
    flashblocks_client: Option<FlashblocksClient>,
    flashblocks_port: Option<u16>,
    block_time_ms: u64,
    flashblocks_block_time_ms: u64,
}

impl BuilderClient {
    /// Create a new builder client.
    pub fn new(
        options: ClientOptions,
        internal: InternalClientOptions,
        port_manager: Arc<PortManager>,
        block_time_ms: u64,
    ) -> Self {
        let fb_block_time = options.flashblocks_block_time_ms.unwrap_or(block_time_ms / 4);
        let inner = BaseRethNodeClient::new(options, internal, port_manager, None);
        Self {
            inner,
            flashblocks_client: None,
            flashblocks_port: None,
            block_time_ms,
            flashblocks_block_time_ms: fb_block_time,
        }
    }
}

#[async_trait]
impl ExecutionClient for BuilderClient {
    async fn run(&mut self) -> Result<(), BenchmarkError> {
        let ws_port = self.inner.port_manager.acquire()?;
        self.flashblocks_port = Some(ws_port);

        self.inner.options.extra_args.extend([
            "--flashblocks.port".into(),
            ws_port.to_string(),
            "--flashblocks.block-time".into(),
            self.flashblocks_block_time_ms.to_string(),
            "--rollup.chain-block-time".into(),
            self.block_time_ms.to_string(),
        ]);

        self.inner.run().await?;
        self.flashblocks_client = Some(FlashblocksClient::new(ws_port));
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), BenchmarkError> {
        self.flashblocks_client = None;
        if let Some(port) = self.flashblocks_port.take() {
            self.inner.port_manager.release(port);
        }
        self.inner.stop().await
    }

    fn rpc_url(&self) -> &str {
        self.inner.rpc_url()
    }

    fn auth_rpc_url(&self) -> &str {
        self.inner.auth_rpc_url()
    }

    fn metrics_port(&self) -> u16 {
        self.inner.metrics_port()
    }

    async fn get_version(&self) -> Result<String, BenchmarkError> {
        self.inner.get_version().await
    }

    async fn set_head(&self, block: u64) -> Result<(), BenchmarkError> {
        self.inner.set_head(block).await
    }

    fn flashblocks_client(&self) -> Option<&FlashblocksClient> {
        self.flashblocks_client.as_ref()
    }

    fn supports_flashblocks(&self) -> bool {
        false
    }
}

/// Construct and start an [`ExecutionClient`] for the given node type,
/// opening `el.log` in the test directory.
pub fn setup_node(
    options: ClientOptions,
    internal: InternalClientOptions,
    port_manager: Arc<PortManager>,
    block_time_ms: u64,
) -> Box<dyn ExecutionClient> {
    match options.node_type.as_str() {
        "builder" => Box::new(BuilderClient::new(options, internal, port_manager, block_time_ms)),
        _ => Box::new(BaseRethNodeClient::new(options, internal, port_manager, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_options(node_type: &str) -> ClientOptions {
        ClientOptions {
            node_type: node_type.into(),
            extra_args: vec![],
            reth_bin: PathBuf::from("base-reth-node"),
            builder_bin: PathBuf::from("base-builder"),
            flashblocks_block_time_ms: None,
        }
    }

    fn make_internal() -> InternalClientOptions {
        InternalClientOptions {
            jwt_secret_path: PathBuf::from("/tmp/jwt"),
            chain_cfg_path: PathBuf::from("/tmp/chain.json"),
            data_dir_path: PathBuf::from("/tmp/data"),
            test_dir_path: PathBuf::from("/tmp/test"),
            jwt_secret: [0u8; 32],
            metrics_path: PathBuf::from("/tmp/metrics"),
        }
    }

    #[test]
    fn build_args_includes_required_flags() {
        let mgr = Arc::new(PortManager::new());
        let client = BaseRethNodeClient::new(make_options("base-reth-node"), make_internal(), mgr, None);
        let args = client.build_args(8545, 8551, 9001, 30303);

        assert!(args.contains(&"node".to_string()));
        assert!(args.contains(&"--http".to_string()));
        assert!(args.contains(&"8545".to_string()));
        assert!(args.contains(&"8551".to_string()));
        assert!(args.contains(&"9001".to_string()));
        assert!(args.contains(&"30303".to_string()));
        assert!(args.contains(&"--disable-discovery".to_string()));
        assert!(args.contains(&"--engine.state-provider-metrics".to_string()));
        assert!(args.contains(&"--db.read-transaction-timeout".to_string()));
    }

    #[test]
    fn build_args_includes_websocket_url() {
        let mgr = Arc::new(PortManager::new());
        let client = BaseRethNodeClient::new(
            make_options("base-reth-node"),
            make_internal(),
            mgr,
            Some("ws://127.0.0.1:9999".into()),
        );
        let args = client.build_args(8545, 8551, 9001, 30303);
        assert!(args.contains(&"--websocket-url".to_string()));
        assert!(args.contains(&"ws://127.0.0.1:9999".to_string()));
    }

    #[test]
    fn build_args_no_websocket_url_by_default() {
        let mgr = Arc::new(PortManager::new());
        let client = BaseRethNodeClient::new(make_options("base-reth-node"), make_internal(), mgr, None);
        let args = client.build_args(8545, 8551, 9001, 30303);
        assert!(!args.contains(&"--websocket-url".to_string()));
    }

    #[test]
    fn builder_extra_args_include_flashblocks_flags() {
        let mgr = Arc::new(PortManager::new());
        let client = BuilderClient::new(make_options("builder"), make_internal(), mgr, 1000);
        assert_eq!(client.flashblocks_block_time_ms, 250);
        assert!(client.inner.options.extra_args.is_empty());
    }

    #[test]
    fn setup_node_dispatches_correctly() {
        let mgr = Arc::new(PortManager::new());
        let node = setup_node(make_options("base-reth-node"), make_internal(), mgr.clone(), 1000);
        assert_eq!(node.supports_flashblocks(), false);
        assert!(node.flashblocks_client().is_none());

        let builder = setup_node(make_options("builder"), make_internal(), mgr, 1000);
        assert_eq!(builder.supports_flashblocks(), false);
    }
}
