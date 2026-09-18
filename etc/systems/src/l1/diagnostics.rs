//! Capture diagnostics for stack-owned L1 containers.

use std::{path::Path, time::Duration};

use eyre::{Result, WrapErr, ensure};
use tokio::{process::Command, time::timeout};

/// Diagnostic capture for one L1 container.
#[derive(Debug)]
pub struct L1Diagnostics;

impl L1Diagnostics {
    /// Writes Docker logs and inspect output for `container_id`.
    pub async fn capture(container_id: &str, directory: &Path, component: &str) -> Result<()> {
        tokio::fs::create_dir_all(directory).await?;
        let mut logs_command = Command::new("docker");
        logs_command.kill_on_drop(true).args(["logs", container_id]);
        let logs = timeout(Duration::from_secs(30), logs_command.output())
            .await
            .wrap_err("Docker log capture timed out")??;
        let mut bytes = logs.stdout;
        bytes.extend_from_slice(&logs.stderr);
        tokio::fs::write(directory.join(format!("{component}.log")), bytes).await?;
        ensure!(logs.status.success(), "Docker log capture failed for {component}");
        let mut inspect_command = Command::new("docker");
        inspect_command.kill_on_drop(true).args(["inspect", container_id]);
        let inspect = timeout(Duration::from_secs(30), inspect_command.output())
            .await
            .wrap_err("Docker inspect timed out")??;
        ensure!(inspect.status.success(), "Docker inspect failed for {component}");
        tokio::fs::write(directory.join(format!("{component}.json")), inspect.stdout).await?;
        Ok(())
    }
}
