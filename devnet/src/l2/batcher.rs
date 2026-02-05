//! op-batcher container for L2 batch submission.
//!
//! The batcher submits L2 transaction batches to L1 for data availability.

use alloy_primitives::B256;
use eyre::{Result, WrapErr, eyre};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

use crate::{
    containers::L2_BATCHER_NAME,
    host::with_host_port_if_needed,
    images::OP_BATCHER_IMAGE,
    l2::L2ContainerConfig,
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
    /// Batcher private key
    pub batcher_key: B256,
}

/// Running op-batcher container.
#[derive(Debug)]
pub struct BatcherContainer {
    _container: ContainerAsync<GenericImage>,
    _name: String,
}

const METRICS_PORT: u16 = 7300;

impl BatcherContainer {
    /// Starts a batcher container with the provided configuration.
    pub async fn start(
        config: BatcherConfig,
        container_config: Option<&L2ContainerConfig>,
    ) -> Result<Self> {
        ensure_network_exists()?;

        let (image_name, image_tag) = OP_BATCHER_IMAGE
            .split_once(':')
            .ok_or_else(|| eyre!("op-batcher image tag is missing"))?;

        let image = GenericImage::new(image_name, image_tag)
            .with_entrypoint("op-batcher")
            .with_wait_for(WaitFor::message_on_stdout("Batch Submitter started"));

        let name = if container_config.is_some_and(|c| c.use_stable_names) {
            L2_BATCHER_NAME.to_string()
        } else {
            unique_name(L2_BATCHER_NAME)
        };

        let network = container_config
            .and_then(|c| c.network_name.clone())
            .unwrap_or_else(|| network_name().to_string());

        let base_container =
            image.with_container_name(&name).with_network(&network).with_cmd(batcher_args(&config));

        let mut container_builder = with_host_port_if_needed(base_container, config.l2_rpc_port);

        if let Some(metrics_port) = container_config.and_then(|c| c.batcher_metrics_port) {
            container_builder =
                container_builder.with_mapped_port(metrics_port, METRICS_PORT.tcp());
        }

        let container =
            container_builder.start().await.wrap_err("Failed to start batcher container")?;

        Ok(Self { _container: container, _name: name })
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
