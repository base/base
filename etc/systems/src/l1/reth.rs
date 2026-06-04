use eyre::{Result, WrapErr, eyre};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use url::Url;

use super::config::L1ContainerConfig;
use crate::{
    containers::L1_RETH_NAME,
    images::RETH_IMAGE,
    network::{ensure_network_exists, ensure_network_exists_with_name, network_name},
    unique_name,
};

const HTTP_PORT: u16 = 8545;
const ENGINE_PORT: u16 = 8551;
const GENESIS_PATH: &str = "/genesis/el/genesis.json";
const JWT_PATH: &str = "/genesis/jwt.hex";

#[derive(Debug)]
/// A container running the Reth execution layer.
pub struct RethContainer {
    container: ContainerAsync<GenericImage>,
    name: String,
}

impl RethContainer {
    /// Starts a new Reth container with the given genesis and JWT secret.
    pub async fn start(
        genesis_json: impl AsRef<[u8]>,
        jwt_secret_hex: impl AsRef<[u8]>,
        config: Option<L1ContainerConfig>,
    ) -> Result<Self> {
        let config = config.unwrap_or_default();

        if let Some(ref net) = config.network_name {
            ensure_network_exists_with_name(net)?;
        } else {
            ensure_network_exists()?;
        }

        let (image_name, image_tag) =
            RETH_IMAGE.split_once(':').ok_or_else(|| eyre!("Reth image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("reth")
            .with_exposed_port(HTTP_PORT.tcp())
            .with_exposed_port(ENGINE_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("RPC HTTP server started"));

        let name = if config.use_stable_names {
            L1_RETH_NAME.to_string()
        } else {
            unique_name(L1_RETH_NAME)
        };
        let network = config.network_name.unwrap_or_else(|| network_name().to_string());

        let mut container_builder = image
            .with_container_name(&name)
            .with_network(&network)
            .with_cmd(reth_args())
            .with_copy_to(GENESIS_PATH, genesis_json.as_ref().to_vec())
            .with_copy_to(JWT_PATH, jwt_secret_hex.as_ref().to_vec());

        if let Some(port) = config.http_port {
            container_builder = container_builder.with_mapped_port(port, HTTP_PORT.tcp());
        }
        if let Some(port) = config.engine_port {
            container_builder = container_builder.with_mapped_port(port, ENGINE_PORT.tcp());
        }

        let container =
            container_builder.start().await.wrap_err("Failed to start Reth container")?;

        Ok(Self { container, name })
    }

    /// Returns the public RPC URL of the container.
    pub async fn rpc_url(&self) -> Result<Url> {
        self.host_url(HTTP_PORT).await
    }

    /// Returns the public Engine API URL of the container.
    pub async fn engine_url(&self) -> Result<Url> {
        self.host_url(ENGINE_PORT).await
    }

    /// Returns the internal RPC URL of the container within the Docker network.
    pub fn internal_rpc_url(&self) -> String {
        format!("http://{}:{}", self.name, HTTP_PORT)
    }

    /// Returns the internal Engine API URL of the container within the Docker network.
    pub fn internal_engine_url(&self) -> String {
        format!("http://{}:{}", self.name, ENGINE_PORT)
    }

    async fn host_url(&self, container_port: u16) -> Result<Url> {
        let host = self.container.get_host().await.wrap_err("Failed to resolve container host")?;
        let host_port = self
            .container
            .get_host_port_ipv4(container_port)
            .await
            .wrap_err("Failed to resolve container port")?;
        Url::parse(&format!("http://{host}:{host_port}")).wrap_err("Failed to build container URL")
    }
}

fn reth_args() -> Vec<&'static str> {
    vec![
        "node",
        "--chain=/genesis/el/genesis.json",
        "--datadir=/data",
        "--http",
        "--http.addr=0.0.0.0",
        "--http.port=8545",
        "--http.api=admin,eth,web3,net,rpc,debug,txpool",
        "--authrpc.port=8551",
        "--authrpc.addr=0.0.0.0",
        "--authrpc.jwtsecret=/genesis/jwt.hex",
        "--disable-discovery",
        "-vvv",
    ]
}
