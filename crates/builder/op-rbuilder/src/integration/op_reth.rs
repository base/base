use crate::integration::{poll_logs, IntegrationError, Service, DEFAULT_JWT_TOKEN};
use futures_util::Future;
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

fn get_or_create_jwt_path(jwt_path: Option<&PathBuf>) -> PathBuf {
    jwt_path.cloned().unwrap_or_else(|| {
        let tmp_dir = std::env::temp_dir();
        let jwt_path = tmp_dir.join("jwt.hex");
        std::fs::write(&jwt_path, DEFAULT_JWT_TOKEN).expect("Failed to write JWT secret file");
        jwt_path
    })
}

#[derive(Default)]
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
    fn ready(&self, log_path: &Path) -> impl Future<Output = Result<(), IntegrationError>> + Send {
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
