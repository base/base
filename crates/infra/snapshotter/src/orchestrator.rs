//! Orchestrates the full snapshot lifecycle with a restart safety guard.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tracing::{error, info, warn};

use crate::{
    SnapshotterConfig,
    container::ContainerManager,
    snapshot::{OutputFileChecksum, SnapshotGenerator, SnapshotManifest, SnapshotManifestExt},
    tip::TipChecker,
    upload::SnapshotUploader,
};

/// Orchestrates the full snapshot flow: stop CL and EL → generate → upload → restart EL and CL.
///
/// Both containers are always restarted, even if snapshot generation or upload
/// fails. This prevents leaving the node in a stopped state on errors and ensures
/// the CL reconnects to the EL after the snapshot.
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
    /// 1. Stops the CL and EL containers
    /// 2. Verifies both containers are stopped
    /// 3. Generates snapshot archives
    /// 4. Uploads to S3/R2
    /// 5. Clears reth's persisted peer list (best effort)
    /// 6. Restarts the EL and then the CL (always, even on failure)
    ///
    /// When `upload_existing_run_timestamp` is set, the snapshotter skips the
    /// container lifecycle entirely and uploads the existing `run-<timestamp>`
    /// directory from `output_dir`.
    pub async fn run(&self) -> Result<()> {
        if let Some(run_timestamp) = self.config.upload_existing_run_timestamp {
            return self.upload_existing_run(run_timestamp).await;
        }

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
                "EL is not at tip; skipping snapshot run and leaving containers running"
            );
            return Ok(());
        }

        // Stop the dependent CL first, then the EL. Restarting in the reverse
        // order below ensures the EL is available when the CL reconnects.
        let cl_stop_result =
            self.container_manager.stop(&self.config.consensus_container_name).await;
        let result = match cl_stop_result {
            Ok(()) => match self.container_manager.stop(&self.config.container_name).await {
                Ok(()) => self.generate_and_upload().await,
                Err(e) => Err(e).context("failed to stop EL container"),
            },
            Err(e) => Err(e).context("failed to stop CL container"),
        };

        // Clear reth's persisted peer list before the EL restarts so the node
        // rediscovers peers from bootnodes — an early-warning canary for peering
        // health. Best effort: a missing file or removal error is logged and
        // never aborts the run or blocks the restart.
        self.clear_known_peers();

        let el_restart_result = self.container_manager.start(&self.config.container_name).await;

        if let Err(ref restart_err) = el_restart_result {
            error!(
                error = %restart_err,
                container = %self.config.container_name,
                "CRITICAL: failed to restart EL container after snapshot"
            );
        }

        let cl_restart_result =
            self.container_manager.start(&self.config.consensus_container_name).await;

        if let Err(ref restart_err) = cl_restart_result {
            error!(
                error = %restart_err,
                container = %self.config.consensus_container_name,
                "CRITICAL: failed to restart CL container after snapshot"
            );
        }

        let restart_result = match (el_restart_result, cl_restart_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(el_err), Ok(())) => Err(el_err).context("failed to restart EL container"),
            (Ok(()), Err(cl_err)) => Err(cl_err).context("failed to restart CL container"),
            (Err(el_err), Err(cl_err)) => {
                bail!("failed to restart EL container ({el_err}) and CL container ({cl_err})")
            }
        };

        match (result, restart_result) {
            (Ok(()), Ok(())) => {
                info!("snapshot lifecycle complete");
                Ok(())
            }
            (Err(snapshot_err), Ok(())) => {
                Err(snapshot_err).context("snapshot failed but EL and CL containers were restarted")
            }
            (Ok(()), Err(restart_err)) => {
                bail!(
                    "snapshot succeeded but container restart failed: {restart_err}. \
                     MANUAL INTERVENTION REQUIRED."
                )
            }
            (Err(snapshot_err), Err(restart_err)) => {
                bail!(
                    "snapshot failed ({snapshot_err}) AND container restart failed \
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

        let previous_chunk_output_files: HashMap<String, Vec<OutputFileChecksum>> = remote_manifest
            .as_ref()
            .map(|manifest| {
                remote_static_files
                    .keys()
                    .filter_map(|filename| {
                        manifest
                            .chunk_output_files_for_file(filename)
                            .map(|output_files| (filename.clone(), output_files))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let source_datadir = self.config.source_datadir.clone();
        let output_dir_for_gen = run_output_dir.clone();
        let chain_id = self.config.chain_id;
        let block = self.config.block;
        let blocks_per_file = self.config.blocks_per_file;
        let remote_for_gen = remote_static_files;
        let previous_chunk_output_files_for_gen = previous_chunk_output_files;

        let files = tokio::task::spawn_blocking(move || {
            SnapshotGenerator::generate_manifest_with_previous_chunk_output_files(
                &source_datadir,
                &output_dir_for_gen,
                chain_id,
                block,
                blocks_per_file,
                &remote_for_gen,
                &previous_chunk_output_files_for_gen,
            )
        })
        .await
        .context("snapshot generation task panicked")?
        .context("snapshot generation failed")?;

        if files.is_empty() {
            bail!("snapshot generation produced no files");
        }

        self.upload_run_directory(&run_output_dir, run_timestamp, files, remote_manifest.as_ref())
            .await?;

        info!(output_dir = %run_output_dir.display(), "cleaning up local artifacts");
        if let Err(e) = tokio::fs::remove_dir_all(&run_output_dir).await {
            error!(error = %e, "failed to clean up output directory");
        }

        Ok(())
    }

    /// Uploads an existing `run-<timestamp>` directory without regenerating artifacts
    /// or touching the EL container lifecycle.
    async fn upload_existing_run(&self, run_timestamp: u64) -> Result<()> {
        let run_output_dir = existing_run_output_dir(&self.config.output_dir, run_timestamp)?;
        info!(
            run_timestamp,
            output_dir = %run_output_dir.display(),
            "uploading existing snapshot run"
        );

        let files = SnapshotGenerator::collect_output_files(&run_output_dir)?;
        let remote_manifest = self.uploader.fetch_previous_manifest().await?;
        info!(
            has_remote_manifest = remote_manifest.is_some(),
            "fetched previous manifest for blake3 diff"
        );
        self.upload_run_directory(&run_output_dir, run_timestamp, files, remote_manifest.as_ref())
            .await
    }

    /// Uploads one prepared run directory after generation or from upload-only mode.
    async fn upload_run_directory(
        &self,
        run_output_dir: &Path,
        run_timestamp: u64,
        files: Vec<PathBuf>,
        remote_manifest: Option<&SnapshotManifest>,
    ) -> Result<()> {
        if files.is_empty() {
            bail!("snapshot run directory produced no files")
        }

        let manifest_bytes = tokio::fs::read(run_output_dir.join("manifest.json"))
            .await
            .context("failed to read run manifest.json")?;
        let local_manifest: SnapshotManifest =
            serde_json::from_slice(&manifest_bytes).context("failed to parse run manifest.json")?;

        self.uploader
            .upload(
                run_output_dir,
                &files,
                run_timestamp,
                self.config.retain_runs.get(),
                &local_manifest,
                remote_manifest,
            )
            .await
            .with_context(|| {
                format!(
                    "snapshot upload failed for run_timestamp={} output_dir={}",
                    run_timestamp,
                    run_output_dir.display()
                )
            })?;
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

/// Resolves an existing `run-<timestamp>` directory for upload-only mode.
fn existing_run_output_dir(base: &Path, timestamp: u64) -> Result<PathBuf> {
    let run_dir = base.join(format!("run-{timestamp}"));
    let metadata = std::fs::metadata(&run_dir)
        .with_context(|| format!("failed to stat existing run dir {}", run_dir.display()))?;
    if !metadata.is_dir() {
        bail!("existing run path is not a directory: {}", run_dir.display());
    }
    Ok(run_dir)
}
