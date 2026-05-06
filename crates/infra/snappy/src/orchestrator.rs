//! Orchestrates the full snapshot lifecycle with a restart safety guard.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tracing::{error, info};

use crate::{
    SnapshotterConfig, container::ContainerManager, snapshot::SnapshotGenerator,
    upload::SnapshotUploader,
};

/// Orchestrates the full snapshot flow: stop EL → generate → upload → restart EL.
///
/// The EL container is always restarted, even if snapshot generation or upload
/// fails. This prevents leaving the node in a stopped state on errors.
pub struct Snapshotter<C: ContainerManager> {
    container_manager: C,
    uploader: SnapshotUploader,
    config: SnapshotterConfig,
}

impl<C: ContainerManager> std::fmt::Debug for Snapshotter<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshotter").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<C: ContainerManager> Snapshotter<C> {
    /// Creates a new snapshotter with the given container manager and uploader.
    pub const fn new(
        container_manager: C,
        uploader: SnapshotUploader,
        config: SnapshotterConfig,
    ) -> Self {
        Self { container_manager, uploader, config }
    }

    /// Executes the full snapshot lifecycle.
    ///
    /// 1. Stops the EL container
    /// 2. Verifies the container is stopped
    /// 3. Generates snapshot archives
    /// 4. Uploads to S3/R2
    /// 5. Restarts the EL container (always, even on failure)
    pub async fn run(&self) -> Result<()> {
        self.container_manager
            .stop(&self.config.container_name)
            .await
            .context("failed to stop EL container")?;

        let result = self.generate_and_upload().await;

        let restart_result = self.container_manager.start(&self.config.container_name).await;

        if let Err(ref restart_err) = restart_result {
            error!(
                error = %restart_err,
                container = %self.config.container_name,
                "CRITICAL: failed to restart EL container after snapshot"
            );
        }

        match (result, restart_result) {
            (Ok(()), Ok(())) => {
                info!("snapshot lifecycle complete");
                Ok(())
            }
            (Err(snapshot_err), Ok(())) => {
                Err(snapshot_err).context("snapshot failed but EL container was restarted")
            }
            (Ok(()), Err(restart_err)) => {
                bail!(
                    "snapshot succeeded but EL container restart failed: {restart_err}. \
                     MANUAL INTERVENTION REQUIRED."
                )
            }
            (Err(snapshot_err), Err(restart_err)) => {
                bail!(
                    "snapshot failed ({snapshot_err}) AND EL container restart failed \
                     ({restart_err}). MANUAL INTERVENTION REQUIRED."
                )
            }
        }
    }

    /// Generates snapshot archives and uploads them. Separated from `run` so
    /// the restart guard logic stays clean.
    async fn generate_and_upload(&self) -> Result<()> {
        let block = self
            .config
            .block
            .ok_or_else(|| anyhow::anyhow!("--block is required for snapshot generation"))?;

        let run_output_dir = create_run_output_dir(&self.config.output_dir)?;

        let files = SnapshotGenerator::generate(
            &self.config.source_datadir,
            &run_output_dir,
            self.config.chain_id,
            block,
            self.config.blocks_per_file,
        )
        .context("snapshot generation failed")?;

        if files.is_empty() {
            bail!("snapshot generation produced no files");
        }

        self.uploader
            .upload(&run_output_dir, &files, block)
            .await
            .context("snapshot upload failed")?;

        info!(output_dir = %run_output_dir.display(), "cleaning up local artifacts");
        if let Err(e) = std::fs::remove_dir_all(&run_output_dir) {
            error!(error = %e, "failed to clean up output directory");
        }

        Ok(())
    }
}

/// Creates a unique run output directory with a timestamp suffix.
fn create_run_output_dir(base: &std::path::Path) -> Result<PathBuf> {
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let run_dir = base.join(format!("run-{timestamp}"));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create run dir {}", run_dir.display()))?;
    Ok(run_dir)
}
