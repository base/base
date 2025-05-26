use std::{
    fs::File,
    future::Future,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::Command,
};

use std::time::Duration;
use tokio::time::sleep;

use super::{
    service::{self, Service},
    DEFAULT_JWT_TOKEN,
};

#[derive(Default, Debug)]
pub struct OpRbuilderConfig {
    auth_rpc_port: Option<u16>,
    jwt_secret_path: Option<PathBuf>,
    chain_config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    http_port: Option<u16>,
    network_port: Option<u16>,
    builder_private_key: Option<String>,
    flashblocks_ws_url: Option<String>,
    chain_block_time: Option<u64>,
    flashbots_block_time: Option<u64>,
    with_revert_protection: Option<bool>,
}

impl OpRbuilderConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn auth_rpc_port(mut self, port: u16) -> Self {
        self.auth_rpc_port = Some(port);
        self
    }

    pub fn chain_config_path<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.chain_config_path = Some(path.into());
        self
    }

    pub fn data_dir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.data_dir = Some(path.into());
        self
    }

    pub fn network_port(mut self, port: u16) -> Self {
        self.network_port = Some(port);
        self
    }

    pub fn http_port(mut self, port: u16) -> Self {
        self.http_port = Some(port);
        self
    }

    pub fn with_builder_private_key(mut self, private_key: &str) -> Self {
        self.builder_private_key = Some(private_key.to_string());
        self
    }

    pub fn with_revert_protection(mut self, revert_protection: bool) -> Self {
        self.with_revert_protection = Some(revert_protection);
        self
    }

    pub fn with_flashblocks_ws_url(mut self, url: &str) -> Self {
        self.flashblocks_ws_url = Some(url.to_string());
        self
    }

    pub fn with_chain_block_time(mut self, time: u64) -> Self {
        self.chain_block_time = Some(time);
        self
    }

    pub fn with_flashbots_block_time(mut self, time: u64) -> Self {
        self.flashbots_block_time = Some(time);
        self
    }
}

impl Service for OpRbuilderConfig {
    fn command(&self) -> Command {
        let mut bin_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        bin_path.push("../../target/debug/op-rbuilder");

        let mut cmd = Command::new(bin_path);
        let jwt_path = get_or_create_jwt_path(self.jwt_secret_path.as_ref());

        cmd.arg("node")
            .arg("--authrpc.port")
            .arg(
                self.auth_rpc_port
                    .expect("auth_rpc_port not set")
                    .to_string(),
            )
            .arg("--authrpc.jwtsecret")
            .arg(
                jwt_path
                    .to_str()
                    .expect("Failed to convert jwt_path to string"),
            )
            .arg("--chain")
            .arg(
                self.chain_config_path
                    .as_ref()
                    .expect("chain_config_path not set"),
            )
            .arg("--datadir")
            .arg(self.data_dir.as_ref().expect("data_dir not set"))
            .arg("--disable-discovery")
            .arg("--color")
            .arg("never")
            .arg("--builder.log-pool-transactions")
            .arg("--port")
            .arg(self.network_port.expect("network_port not set").to_string())
            .arg("--ipcdisable")
            .arg("-vvvv");

        if let Some(revert_protection) = self.with_revert_protection {
            if revert_protection {
                cmd.arg("--builder.enable-revert-protection");
            }
        }

        if let Some(builder_private_key) = &self.builder_private_key {
            cmd.arg("--rollup.builder-secret-key")
                .arg(builder_private_key);
        }

        if let Some(http_port) = self.http_port {
            cmd.arg("--http")
                .arg("--http.port")
                .arg(http_port.to_string())
                .arg("--http.api")
                .arg("eth,web3,txpool");
        }

        if let Some(flashblocks_ws_url) = &self.flashblocks_ws_url {
            cmd.arg("--rollup.enable-flashblocks").arg("true");

            cmd.arg("--rollup.flashblocks-ws-url")
                .arg(flashblocks_ws_url);
        }

        if let Some(chain_block_time) = self.chain_block_time {
            cmd.arg("--rollup.chain-block-time")
                .arg(chain_block_time.to_string());
        }

        if let Some(flashbots_block_time) = self.flashbots_block_time {
            cmd.arg("--rollup.flashblock-block-time")
                .arg(flashbots_block_time.to_string());
        }

        cmd
    }

    #[allow(clippy::manual_async_fn)]
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), service::Error>> + Send {
        async move {
            poll_logs(
                log_path,
                "Starting consensus engine",
                Duration::from_millis(100),
                Duration::from_secs(60),
            )
            .await
        }
    }
}

#[derive(Default, Debug)]
pub struct OpRethConfig {
    auth_rpc_port: Option<u16>,
    jwt_secret_path: Option<PathBuf>,
    chain_config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    http_port: Option<u16>,
    network_port: Option<u16>,
}

impl OpRethConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn auth_rpc_port(mut self, port: u16) -> Self {
        self.auth_rpc_port = Some(port);
        self
    }

    pub fn chain_config_path<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.chain_config_path = Some(path.into());
        self
    }

    pub fn data_dir<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.data_dir = Some(path.into());
        self
    }

    pub fn network_port(mut self, port: u16) -> Self {
        self.network_port = Some(port);
        self
    }
}

impl Service for OpRethConfig {
    fn command(&self) -> Command {
        let bin_path = PathBuf::from("op-reth");

        let mut cmd = Command::new(bin_path);
        let jwt_path = get_or_create_jwt_path(self.jwt_secret_path.as_ref());

        cmd.arg("node")
            .arg("--authrpc.port")
            .arg(
                self.auth_rpc_port
                    .expect("auth_rpc_port not set")
                    .to_string(),
            )
            .arg("--authrpc.jwtsecret")
            .arg(
                jwt_path
                    .to_str()
                    .expect("Failed to convert jwt_path to string"),
            )
            .arg("--chain")
            .arg(
                self.chain_config_path
                    .as_ref()
                    .expect("chain_config_path not set"),
            )
            .arg("--datadir")
            .arg(self.data_dir.as_ref().expect("data_dir not set"))
            .arg("--disable-discovery")
            .arg("--color")
            .arg("never")
            .arg("--port")
            .arg(self.network_port.expect("network_port not set").to_string())
            .arg("--ipcdisable");

        if let Some(http_port) = self.http_port {
            cmd.arg("--http")
                .arg("--http.port")
                .arg(http_port.to_string());
        }

        cmd
    }

    #[allow(clippy::manual_async_fn)]
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), service::Error>> + Send {
        async move {
            poll_logs(
                log_path,
                "Starting consensus engine",
                Duration::from_millis(100),
                Duration::from_secs(60),
            )
            .await
        }
    }
}

fn get_or_create_jwt_path(jwt_path: Option<&PathBuf>) -> PathBuf {
    jwt_path.cloned().unwrap_or_else(|| {
        let tmp_dir = std::env::temp_dir();
        let jwt_path = tmp_dir.join("jwt.hex");
        std::fs::write(&jwt_path, DEFAULT_JWT_TOKEN).expect("Failed to write JWT secret file");
        jwt_path
    })
}

/// Helper function to poll logs periodically
pub async fn poll_logs(
    log_path: &Path,
    pattern: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<(), service::Error> {
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(service::Error::Spawn(ErrorKind::TimedOut));
        }

        let mut file = File::open(log_path).map_err(|_| service::Error::Logs)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|_| service::Error::Logs)?;

        if contents.contains(pattern) {
            return Ok(());
        }

        sleep(interval).await;
    }
}
