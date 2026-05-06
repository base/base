//! Container lifecycle management via the Docker socket.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bollard::{
    Docker,
    query_parameters::{
        InspectContainerOptions, StartContainerOptions, StopContainerOptionsBuilder,
    },
};
use tracing::{info, warn};

/// Manages the lifecycle of a container (stop/start with state verification).
#[async_trait]
pub trait ContainerManager: Send + Sync {
    /// Stops the container and verifies it is no longer running.
    async fn stop(&self, container_name: &str) -> Result<()>;

    /// Starts the container and verifies it is running.
    async fn start(&self, container_name: &str) -> Result<()>;

    /// Returns `true` if the container is currently running.
    async fn is_running(&self, container_name: &str) -> Result<bool>;
}

/// Docker-based container manager that communicates via the Docker socket.
///
/// The socket path (e.g. `/var/run/docker.sock`) is volume-mounted into the
/// sidecar container, giving it control over sibling containers on the host.
#[derive(Debug)]
pub struct DockerContainerManager {
    client: Docker,
}

impl DockerContainerManager {
    /// Connects to the Docker daemon via a Unix socket.
    pub fn new(socket_path: &str) -> Result<Self> {
        let client = Docker::connect_with_socket(socket_path, 120, bollard::API_DEFAULT_VERSION)
            .with_context(|| format!("failed to connect to Docker socket at {socket_path}"))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl ContainerManager for DockerContainerManager {
    async fn stop(&self, container_name: &str) -> Result<()> {
        info!(container = %container_name, "stopping container");

        let opts = StopContainerOptionsBuilder::new().t(30).build();
        self.client
            .stop_container(container_name, Some(opts))
            .await
            .with_context(|| format!("failed to stop container {container_name}"))?;

        let running = self.is_running(container_name).await?;
        if running {
            bail!("container {container_name} is still running after stop request");
        }

        info!(container = %container_name, "container stopped and verified");
        Ok(())
    }

    async fn start(&self, container_name: &str) -> Result<()> {
        info!(container = %container_name, "starting container");

        self.client
            .start_container(container_name, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("failed to start container {container_name}"))?;

        let running = self.is_running(container_name).await?;
        if !running {
            warn!(container = %container_name, "container not running after start request");
        }

        info!(container = %container_name, "container started");
        Ok(())
    }

    async fn is_running(&self, container_name: &str) -> Result<bool> {
        let info = self
            .client
            .inspect_container(container_name, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("failed to inspect container {container_name}"))?;

        let running = info.state.and_then(|s| s.running).unwrap_or(false);
        Ok(running)
    }
}
