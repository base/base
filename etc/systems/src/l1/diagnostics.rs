//! Capture Docker-owned L1 evidence before scoped container cleanup.

use std::{path::Path, time::Duration};

use eyre::{Result, WrapErr, ensure};
use tokio::{process::Command, time::timeout};

/// Evidence collection for an explicitly owned L1 container.
#[derive(Debug)]
pub struct L1Diagnostics;

impl L1Diagnostics {
    /// Retains full client logs and Docker identity/configuration for one owned container.
    pub async fn capture(container_id: &str, directory: &Path, component: &str) -> Result<()> {
        tokio::fs::create_dir_all(directory).await?;
        let logs = timeout(
            Duration::from_secs(30),
            Command::new("docker").args(["logs", container_id]).output(),
        )
        .await
        .wrap_err("Docker log capture timed out")??;
        let mut bytes = logs.stdout;
        bytes.extend_from_slice(&logs.stderr);
        tokio::fs::write(directory.join(format!("{component}.log")), bytes).await?;
        ensure!(logs.status.success(), "Docker log capture failed for {component}");
        let inspect = timeout(
            Duration::from_secs(30),
            Command::new("docker").args(["inspect", container_id]).output(),
        )
        .await
        .wrap_err("Docker identity capture timed out")??;
        ensure!(inspect.status.success(), "Docker identity capture failed for {component}");
        tokio::fs::write(directory.join(format!("{component}.json")), inspect.stdout).await?;
        Ok(())
    }
}
