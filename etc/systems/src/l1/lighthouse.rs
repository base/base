//! Lighthouse beacon and validator containers.

use std::path::Path;

use eyre::Result;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};

use super::config::L1ContainerConfig;
use crate::{
    containers::{L1_BEACON_HTTP_PORT, L1_BEACON_NAME, L1_VALIDATOR_NAME},
    network::{ensure_network_exists, ensure_network_exists_with_name, network_name},
    unique_name,
};

const LIGHTHOUSE_IMAGE_NAME: &str = "sigp/lighthouse";
const LIGHTHOUSE_IMAGE_TAG: &str = "v8.2.2";
const LIGHTHOUSE_TESTNET_DIR: &str = "/genesis/cl";
const LIGHTHOUSE_JWT_PATH: &str = "/genesis/jwt.hex";
const LIGHTHOUSE_BEACON_DATA_DIR: &str = "/data/beacon";
const LIGHTHOUSE_VALIDATOR_DATA_DIR: &str = "/genesis/cl/validator_data";
const LIGHTHOUSE_HTTP_PORT: u16 = 4052;

/// Lighthouse beacon node container wrapper.
#[derive(Debug)]
pub struct LighthouseBeaconContainer {
    container: ContainerAsync<GenericImage>,
    name: String,
}

impl LighthouseBeaconContainer {
    /// Starts a Lighthouse beacon node.
    pub async fn start(
        testnet_dir: impl AsRef<Path>,
        jwt_path: impl AsRef<Path>,
        execution_endpoint: impl AsRef<str>,
        config: Option<L1ContainerConfig>,
    ) -> Result<Self> {
        let config = config.unwrap_or_default();

        if let Some(ref net) = config.network_name {
            ensure_network_exists_with_name(net)?;
        } else {
            ensure_network_exists()?;
        }

        let command = beacon_command(execution_endpoint.as_ref());
        let image = lighthouse_image()
            .with_exposed_port(LIGHTHOUSE_HTTP_PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("HTTP API started"));

        let name = if config.use_stable_names {
            L1_BEACON_NAME.to_string()
        } else {
            unique_name(L1_BEACON_NAME)
        };
        let network = config.network_name.unwrap_or_else(|| network_name().to_string());

        let mut container_builder = image
            .with_container_name(&name)
            .with_network(&network)
            .with_mount(Mount::bind_mount(
                path_for_mount(testnet_dir.as_ref()),
                LIGHTHOUSE_TESTNET_DIR,
            ))
            .with_mount(Mount::bind_mount(path_for_mount(jwt_path.as_ref()), LIGHTHOUSE_JWT_PATH))
            .with_cmd(command);

        if config.tmpfs_datadir {
            // Back the beacon datadir with tmpfs so its mmap-backed database works on hosts where
            // the container's overlayfs upper layer rejects writable mmap (e.g. docker-in-docker CI).
            container_builder = container_builder.with_mount(Mount::tmpfs_mount("/data"));
        }

        if let Some(port) = config.beacon_http_port {
            container_builder =
                container_builder.with_mapped_port(port, LIGHTHOUSE_HTTP_PORT.tcp());
        }

        let container = container_builder.start().await?;

        Ok(Self { container, name })
    }

    /// Returns the beacon API URL (host-accessible).
    pub async fn beacon_url(&self) -> Result<String> {
        let host = self.container.get_host().await?;
        let port = self.container.get_host_port_ipv4(LIGHTHOUSE_HTTP_PORT.tcp()).await?;
        Ok(format!("http://{host}:{port}"))
    }

    /// Returns the internal beacon API URL for inter-container communication.
    pub fn internal_beacon_url(&self) -> String {
        format!("http://{}:{}", self.name, L1_BEACON_HTTP_PORT)
    }

    /// Stops the beacon node so it cannot overwrite a test-controlled execution forkchoice.
    pub async fn stop(&self) -> Result<()> {
        self.container.stop().await?;
        Ok(())
    }
}

/// Lighthouse validator client container wrapper.
#[derive(Debug)]
pub struct LighthouseValidatorContainer {
    container: ContainerAsync<GenericImage>,
}

impl LighthouseValidatorContainer {
    /// Starts a Lighthouse validator client.
    pub async fn start(
        testnet_dir: impl AsRef<Path>,
        validator_keystores: impl AsRef<Path>,
        beacon_endpoint: impl AsRef<str>,
        config: Option<L1ContainerConfig>,
    ) -> Result<Self> {
        let config = config.unwrap_or_default();

        if let Some(ref net) = config.network_name {
            ensure_network_exists_with_name(net)?;
        } else {
            ensure_network_exists()?;
        }

        let command = validator_command(beacon_endpoint.as_ref());
        let image = lighthouse_image();

        let name = if config.use_stable_names {
            L1_VALIDATOR_NAME.to_string()
        } else {
            unique_name(L1_VALIDATOR_NAME)
        };
        let network = config.network_name.unwrap_or_else(|| network_name().to_string());

        let container = image
            .with_container_name(&name)
            .with_network(&network)
            .with_mount(Mount::bind_mount(
                path_for_mount(testnet_dir.as_ref()),
                LIGHTHOUSE_TESTNET_DIR,
            ))
            .with_mount(Mount::bind_mount(
                path_for_mount(validator_keystores.as_ref()),
                LIGHTHOUSE_VALIDATOR_DATA_DIR,
            ))
            .with_cmd(command)
            .start()
            .await?;

        Ok(Self { container })
    }

    /// Stops the validator before its beacon node is stopped for fault injection.
    pub async fn stop(&self) -> Result<()> {
        self.container.stop().await?;
        Ok(())
    }
}

fn lighthouse_image() -> GenericImage {
    GenericImage::new(LIGHTHOUSE_IMAGE_NAME, LIGHTHOUSE_IMAGE_TAG).with_entrypoint("lighthouse")
}

fn beacon_command(execution_endpoint: &str) -> Vec<String> {
    vec![
        "bn".to_string(),
        format!("--testnet-dir={LIGHTHOUSE_TESTNET_DIR}"),
        format!("--datadir={LIGHTHOUSE_BEACON_DATA_DIR}"),
        "--enable-private-discovery".to_string(),
        "--disable-peer-scoring".to_string(),
        "--staking".to_string(),
        "--http".to_string(),
        "--http-address=0.0.0.0".to_string(),
        format!("--http-port={LIGHTHOUSE_HTTP_PORT}"),
        "--http-allow-origin=*".to_string(),
        "--target-peers=0".to_string(),
        "--supernode".to_string(),
        format!("--execution-endpoint={execution_endpoint}"),
        format!("--execution-jwt={LIGHTHOUSE_JWT_PATH}"),
    ]
}

fn validator_command(beacon_endpoint: &str) -> Vec<String> {
    vec![
        "vc".to_string(),
        format!("--testnet-dir={LIGHTHOUSE_TESTNET_DIR}"),
        format!("--datadir={LIGHTHOUSE_VALIDATOR_DATA_DIR}"),
        format!("--beacon-nodes={beacon_endpoint}"),
        "--init-slashing-protection".to_string(),
    ]
}

fn path_for_mount(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
