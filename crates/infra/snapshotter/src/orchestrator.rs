//! Orchestrates the full snapshot lifecycle with a restart safety guard.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

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
        let stop_result = self.container_manager.stop(&self.config.container_name).await;

        let result = match stop_result {
            Ok(()) => self.generate_and_upload().await,
            Err(e) => Err(e).context("failed to stop EL container"),
        };

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
        let run_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let run_output_dir = create_run_output_dir(&self.config.output_dir, run_timestamp)?;

        let source_datadir = self.config.source_datadir.clone();
        let output_dir_for_gen = run_output_dir.clone();
        let chain_id = self.config.chain_id;
        let block = self.config.block;
        let blocks_per_file = self.config.blocks_per_file;

        let files = tokio::task::spawn_blocking(move || {
            SnapshotGenerator::generate(
                &source_datadir,
                &output_dir_for_gen,
                chain_id,
                block,
                blocks_per_file,
            )
        })
        .await
        .context("snapshot generation task panicked")?
        .context("snapshot generation failed")?;

        if files.is_empty() {
            bail!("snapshot generation produced no files");
        }

        self.uploader
            .upload(&run_output_dir, &files, run_timestamp)
            .await
            .context("snapshot upload failed")?;

        info!(output_dir = %run_output_dir.display(), "cleaning up local artifacts");
        if let Err(e) = std::fs::remove_dir_all(&run_output_dir) {
            error!(error = %e, "failed to clean up output directory");
        }

        Ok(())
    }
}

/// Creates a unique run output directory using the provided timestamp.
fn create_run_output_dir(base: &std::path::Path, timestamp: u64) -> Result<PathBuf> {
    let run_dir = base.join(format!("run-{timestamp}"));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create run dir {}", run_dir.display()))?;
    Ok(run_dir)
}
