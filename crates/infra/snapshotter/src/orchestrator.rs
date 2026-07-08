//! Orchestrates the full snapshot lifecycle with a restart safety guard.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tracing::{error, info, warn};

use crate::{
    SnapshotterConfig,
    container::ContainerManager,
    snapshot::{SnapshotGenerator, SnapshotManifest},
    tip::TipChecker,
    upload::SnapshotUploader,
};

/// Orchestrates the full snapshot flow: stop EL → generate → upload → restart EL.
///
/// The EL container is always restarted, even if snapshot generation or upload
/// fails. This prevents leaving the node in a stopped state on errors.
pub struct Snapshotter<C: ContainerManager, T: TipChecker> {
    container_manager: C,
    tip_checker: T,
    uploader: SnapshotUploader,
    config: SnapshotterConfig,
}

impl<C: ContainerManager, T: TipChecker> std::fmt::Debug for Snapshotter<C, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshotter").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<C: ContainerManager, T: TipChecker> Snapshotter<C, T> {
    /// Creates a new snapshotter with the given container manager, tip checker,
    /// and uploader.
    pub const fn new(
        container_manager: C,
        tip_checker: T,
        uploader: SnapshotUploader,
        config: SnapshotterConfig,
    ) -> Self {
        Self { container_manager, tip_checker, uploader, config }
    }

    /// Executes the full snapshot lifecycle.
    ///
    /// 0. Verifies the EL is at chain tip; skips the run if it is not
    /// 1. Stops the EL container
    /// 2. Verifies the container is stopped
    /// 3. Generates snapshot archives
    /// 4. Uploads to S3/R2
    /// 5. Clears reth's persisted peer list (best effort)
    /// 6. Restarts the EL container (always, even on failure)
    pub async fn run(&self) -> Result<()> {
        // Only snapshot when the EL is caught up to tip. Snapshotting a lagging
        // node would publish stale data and pause a node that is still syncing.
        //
        // This is a best-effort PRE-check, not a guarantee of freshness at
        // snapshot time. There is an inherent TOCTOU gap: after this check
        // passes, time elapses while we stop the container, generate archives,
        // and upload — so a node that was "barely at tip" (e.g. 9s old with a
        // 10s threshold) may be stale by the time data is actually captured.
        // This is acceptable for the default 10s threshold on a 2s block-time
        // chain, but callers tightening the threshold should keep this in mind.
        let threshold = Duration::from_secs(self.config.tip_threshold_secs);
        let at_tip =
            self.tip_checker.is_at_tip(threshold).await.context("failed to check EL tip status")?;
        if !at_tip {
            warn!(
                threshold_secs = self.config.tip_threshold_secs,
                "EL is not at tip; skipping snapshot run and leaving container running"
            );
            return Ok(());
        }

        let stop_result = self.container_manager.stop(&self.config.container_name).await;

        let result = match stop_result {
            Ok(()) => self.generate_and_upload().await,
            Err(e) => Err(e).context("failed to stop EL container"),
        };

        // Clear reth's persisted peer list before the EL restarts so the node
        // rediscovers peers from bootnodes — an early-warning canary for peering
        // health. Best effort: a missing file or removal error is logged and
        // never aborts the run or blocks the restart.
        self.clear_known_peers();

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

        let remote_static_files = self.uploader.list_remote_static_files().await?;

        info!(remote_files = remote_static_files.len(), "fetched remote static file listing");

        let remote_manifest = self.uploader.fetch_previous_manifest().await?;
        info!(
            has_remote_manifest = remote_manifest.is_some(),
            "fetched previous manifest for blake3 diff"
        );

        let source_datadir = self.config.source_datadir.clone();
        let output_dir_for_gen = run_output_dir.clone();
        let chain_id = self.config.chain_id;
        let block = self.config.block;
        let blocks_per_file = self.config.blocks_per_file;
        let remote_for_gen = remote_static_files;

        let files = tokio::task::spawn_blocking(move || {
            SnapshotGenerator::generate_manifest(
                &source_datadir,
                &output_dir_for_gen,
                chain_id,
                block,
                blocks_per_file,
                &remote_for_gen,
            )
        })
        .await
        .context("snapshot generation task panicked")?
        .context("snapshot generation failed")?;

        if files.is_empty() {
            bail!("snapshot generation produced no files");
        }

        let manifest_bytes = tokio::fs::read(run_output_dir.join("manifest.json"))
            .await
            .context("failed to read freshly generated manifest.json")?;
        let local_manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .context("failed to parse freshly generated manifest.json")?;

        self.uploader
            .upload(
                &run_output_dir,
                &files,
                run_timestamp,
                self.config.retain_runs.get(),
                &local_manifest,
                remote_manifest.as_ref(),
            )
            .await
            .context("snapshot upload failed")?;

        info!(output_dir = %run_output_dir.display(), "cleaning up local artifacts");
        if let Err(e) = tokio::fs::remove_dir_all(&run_output_dir).await {
            error!(error = %e, "failed to clean up output directory");
        }

        Ok(())
    }

    /// Removes reth's persisted peer list (`known-peers.json`) from the datadir.
    ///
    /// Best effort: a missing file or removal error is logged and swallowed so
    /// it never aborts the snapshot run or blocks the EL restart.
    fn clear_known_peers(&self) {
        let known_peers = self.config.source_datadir.join("known-peers.json");
        match std::fs::remove_file(&known_peers) {
            Ok(()) => info!(path = %known_peers.display(), "cleared persisted peer list"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                error!(path = %known_peers.display(), "persisted peer list not found; nothing to clear")
            }
            Err(e) => {
                error!(error = %e, path = %known_peers.display(), "failed to clear persisted peer list")
            }
        }
    }
}

/// Creates a unique run output directory using the provided timestamp.
fn create_run_output_dir(base: &std::path::Path, timestamp: u64) -> Result<PathBuf> {
    let run_dir = base.join(format!("run-{timestamp}"));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("failed to create run dir {}", run_dir.display()))?;
    Ok(run_dir)
}
