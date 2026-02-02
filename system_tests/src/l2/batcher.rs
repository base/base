//! op-batcher container for L2 batch submission.
//!
//! The batcher submits L2 transaction batches to L1 for data availability.

use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::WaitFor,
    runners::AsyncRunner,
};

const CONTAINER_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

use crate::{
    containers::L2_BATCHER_NAME,
    images::OP_BATCHER_IMAGE,
    network::{ensure_network_exists, network_name},
    unique_name,
};

/// Configuration for starting a batcher container.
#[derive(Debug, Clone)]
pub struct BatcherConfig {
    /// L1 RPC URL for submitting batches.
    pub l1_rpc_url: String,
    /// L2 RPC URL for reading transactions.
    pub l2_rpc_url: String,
    /// L2 RPC port on host (for testcontainers host port exposure).
    pub l2_rpc_port: u16,
    /// Rollup RPC URL (op-node).
    pub rollup_rpc_url: String,
    /// Batcher private key (hex-encoded string, e.g., "0x...").
    pub batcher_key: String,
}

/// Running op-batcher container.
#[derive(Debug)]
pub struct BatcherContainer {
    container: ContainerAsync<GenericImage>,
    #[allow(dead_code)]
    name: String,
}

impl BatcherContainer {
    /// Starts a batcher container with the provided configuration.
    pub async fn start(config: BatcherConfig) -> Result<Self> {
        ensure_network_exists()?;

        let (image_name, image_tag) = OP_BATCHER_IMAGE
            .split_once(':')
            .ok_or_else(|| eyre!("op-batcher image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("op-batcher")
            .with_wait_for(WaitFor::message_on_stdout("Batch Submitter started"));

        let name = unique_name(L2_BATCHER_NAME);

        let container = image
            .with_container_name(&name)
            .with_network(network_name())
            .with_exposed_host_port(config.l2_rpc_port)
            .with_cmd(batcher_args(&config))
            .with_startup_timeout(CONTAINER_STARTUP_TIMEOUT)
            .start()
            .await
            .wrap_err("Failed to start batcher container")?;

        Ok(Self { container, name })
    }

    #[allow(dead_code)]
    pub(crate) const fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }
}

fn batcher_args(config: &BatcherConfig) -> Vec<String> {
    vec![
        format!("--l1-eth-rpc={}", config.l1_rpc_url),
        format!("--l2-eth-rpc={}", config.l2_rpc_url),
        format!("--rollup-rpc={}", config.rollup_rpc_url),
        format!("--private-key={}", config.batcher_key),
        "--sub-safety-margin=4".to_string(),
        "--poll-interval=1s".to_string(),
        "--num-confirmations=1".to_string(),
        "--max-channel-duration=2".to_string(),
    ]
}
