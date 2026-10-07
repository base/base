//! Orchestrates the full snapshot lifecycle with a restart safety guard.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base_reth_cli::{ChunkFilename, ManifestGenerationParams, SnapshotGenerator};
use tracing::{error, info, warn};

use crate::{
    SnapshotterConfig,
    container::ContainerManager,
    tip::TipChecker,
    upload::{SnapshotUploader, StreamingS3ArchiveSink},
};

/// Orchestrates the full snapshot flow: optionally stop CL, stop EL → generate →
/// upload → restart EL, then optionally restart CL.
///
/// The EL is always restarted, even if snapshot generation or upload fails. When a
/// CL container is configured, it is stopped first and restarted last so it can
/// reconnect to the EL. This prevents leaving the node in a stopped state on errors.
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
    /// 0. Captures the EL's latest block and verifies it is at chain tip; skips the run if it is not
    ///    When uploading proofs, also verifies proofs lag is within the configured block limit
    /// 1. Stops the CL (when configured) then the EL
    /// 2. Verifies stopped containers are no longer running
    /// 3. Generates snapshot archives
    /// 4. Uploads to S3/R2
    /// 5. Clears reth's persisted peer list (best effort)
    /// 6. Restarts the EL and then the CL when configured (always, even on failure)
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
        let tip =
            self.tip_checker.check_tip(threshold).await.context("failed to check EL tip status")?;
        if !tip.at_tip {
            warn!(
                threshold_secs = self.config.tip_threshold_secs,
                "EL is not at tip; skipping snapshot run and leaving containers running"
            );
            return Ok(());
        }

        if self.config.upload_proofs {
            let proofs_latest = self
                .tip_checker
                .proofs_latest()
                .await
                .context("failed to check proofs sync status")?;
            let Some(proofs_latest) = proofs_latest else {
                warn!(
                    "proofs database is empty; skipping snapshot run and leaving containers running"
                );
                return Ok(());
            };
            // Proofs can advance beyond the sampled EL head between the two RPC calls.
            let lag_blocks = tip.block_number.saturating_sub(proofs_latest);
            if lag_blocks > self.config.proofs_max_lag_blocks {
                warn!(
                    el_block = tip.block_number,
                    proofs_latest,
                    lag_blocks,
                    max_lag_blocks = self.config.proofs_max_lag_blocks,
                    "proofs sync is too far behind; skipping snapshot run and leaving containers running"
                );
                return Ok(());
            }
            info!(
                el_block = tip.block_number,
                proofs_latest,
                lag_blocks,
                max_lag_blocks = self.config.proofs_max_lag_blocks,
                "checked proofs sync status"
            );
        }

        // Stop the dependent CL first when configured, then the EL. Restarting
        // in the reverse order below ensures the EL is available when the CL
        // reconnects.
        let cl_stop_result = if let Some(ref cl_name) = self.config.consensus_container_name {
            self.container_manager.stop(cl_name).await
        } else {
            Ok(())
        };
        let result = match cl_stop_result {
            Ok(()) => match self.container_manager.stop(&self.config.container_name).await {
                Ok(()) => self.generate_and_upload(tip.block_number).await,
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

        let cl_restart_result = if let Some(ref cl_name) = self.config.consensus_container_name {
            let cl_restart_result = self.container_manager.start(cl_name).await;
            if let Err(ref restart_err) = cl_restart_result {
                error!(
                    error = %restart_err,
                    container = %cl_name,
                    "CRITICAL: failed to restart CL container after snapshot"
                );
            }
            cl_restart_result
        } else {
            Ok(())
        };

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
                let restarted = if self.config.consensus_container_name.is_some() {
                    "snapshot failed but EL and CL containers were restarted"
                } else {
                    "snapshot failed but EL container was restarted"
                };
                Err(snapshot_err).context(restarted)
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
    async fn generate_and_upload(&self, latest_block: u64) -> Result<()> {
        let run_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let remote_static_files = self.uploader.list_remote_static_files().await?;

        info!(remote_files = remote_static_files.len(), "fetched remote static file listing");

        let remote_manifest = self.uploader.fetch_previous_manifest().await?;
        info!(
            has_remote_manifest = remote_manifest.is_some(),
            "fetched previous manifest for blake3 diff"
        );

        let source_datadir = self.config.source_datadir.clone();
        let chain_id = self.config.chain_id;
        let block = self.config.block.unwrap_or(latest_block);
        let blocks_per_file = self.config.blocks_per_file;
        let remote_for_gen = remote_static_files.clone();
        let previous_manifest_for_gen = remote_manifest.clone();
        let upload_proofs = self.config.upload_proofs;
        let effective_block = block;
        let effective_blocks_per_file = blocks_per_file.unwrap_or(500_000);
        let latest_chunk_start = effective_block
            .saturating_sub(1)
            .checked_div(effective_blocks_per_file)
            .and_then(|index| index.checked_mul(effective_blocks_per_file))
            .context("latest static-file chunk range overflow")?;
        let key_uploader = self.uploader.clone();
        let sink = StreamingS3ArchiveSink::new(
            self.uploader.clone(),
            tokio::runtime::Handle::current(),
            self.config.max_streaming_archives.get(),
            move |archive_name| {
                let key = match ChunkFilename::parse(archive_name) {
                    Some((_component, start, _end)) if start != latest_chunk_start => {
                        key_uploader.static_file_object_key(archive_name)
                    }
                    _ => key_uploader.run_object_key(run_timestamp, archive_name),
                };
                Ok(key)
            },
        )?;

        let manifest = tokio::task::spawn_blocking(move || {
            let params = ManifestGenerationParams {
                source_datadir: &source_datadir,
                output_dir: None,
                chain_id,
                base_url: None,
                block: Some(block),
                blocks_per_file,
                remote_static_files: &remote_for_gen,
                previous_manifest: previous_manifest_for_gen.as_ref(),
                upload_proofs,
            };
            SnapshotGenerator::generate_manifest_with_sink(&params, &sink)
        })
        .await
        .context("snapshot generation task panicked")?
        .context("snapshot generation failed")?;

        self.uploader
            .publish_streamed_manifest(&manifest, run_timestamp, self.config.retain_runs.get())
            .await
            .with_context(|| {
                format!("failed to publish streamed snapshot manifest for {run_timestamp}")
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use aws_sdk_s3::{
        Client,
        config::{BehaviorVersion, Credentials, Region},
    };
    use clap::Parser;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        container::MockContainerManager,
        tip::{MockTipChecker, TipStatus},
    };

    /// Parses snapshotter arguments through the public CLI configuration.
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        config: SnapshotterConfig,
    }

    /// Builds a snapshotter with an unreachable uploader; pre-checks must not access S3.
    fn snapshotter(
        container_manager: MockContainerManager,
        tip_checker: MockTipChecker,
        upload_proofs: bool,
        max_lag: u64,
    ) -> (Snapshotter<MockContainerManager, MockTipChecker>, TempDir) {
        let tmp = TempDir::new().expect("temporary datadir should be created");
        let mut config = TestCli::try_parse_from([
            "snapshotter",
            "--container-name",
            "el",
            "--consensus-container-name",
            "cl",
            "--el-rpc-url",
            "http://127.0.0.1:1",
            "--source-datadir",
            tmp.path().to_str().expect("test path should be UTF-8"),
            "--bucket",
            "snapshots",
        ])
        .expect("test configuration should parse")
        .config;
        config.upload_proofs = upload_proofs;
        config.proofs_max_lag_blocks = max_lag;
        let client = Client::from_conf(
            aws_sdk_s3::config::Builder::new()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new("test", "test", None, None, "test"))
                .endpoint_url("http://127.0.0.1:1")
                .build(),
        );
        let uploader = SnapshotUploader::new(client, "snapshots".into(), String::new(), None);
        (Snapshotter::new(container_manager, tip_checker, uploader, config), tmp)
    }

    /// Verifies lagging or empty proofs leave both containers, peer data, and S3 untouched.
    #[tokio::test]
    async fn lagging_or_empty_proofs_skip_snapshot() {
        for (latest, max_lag) in
            [(Some(999), 1_000), (None, 1_000), (Some(1_999), 0), (Some(1_989), 10)]
        {
            let mut containers = MockContainerManager::new();
            containers.expect_stop().never();
            containers.expect_start().never();
            containers.expect_is_running().never();
            let mut tip = MockTipChecker::new();
            tip.expect_check_tip()
                .times(1)
                .returning(|_| Ok(TipStatus { block_number: 2_000, at_tip: true }));
            tip.expect_proofs_latest().times(1).returning(move || Ok(latest));
            let (snapshotter, tmp) = snapshotter(containers, tip, true, max_lag);
            let peers = tmp.path().join("known-peers.json");
            std::fs::write(&peers, b"peers").expect("peer data should be written");
            snapshotter.run().await.expect("lagging or empty proofs should skip successfully");
            assert_eq!(
                std::fs::read(peers).expect("peer data must remain"),
                b"peers",
                "skipped runs must not clear peers"
            );
        }
    }

    /// Verifies RPC failures abort before stopping either container or accessing S3.
    #[tokio::test]
    async fn proofs_rpc_failure_aborts_before_container_stop() {
        let mut containers = MockContainerManager::new();
        containers.expect_stop().never();
        containers.expect_start().never();
        containers.expect_is_running().never();
        let mut tip = MockTipChecker::new();
        tip.expect_check_tip()
            .times(1)
            .returning(|_| Ok(TipStatus { block_number: 2_000, at_tip: true }));
        tip.expect_proofs_latest().times(1).returning(|| Err(anyhow!("RPC unavailable")));
        let (snapshotter, _tmp) = snapshotter(containers, tip, true, 1_000);
        let err =
            snapshotter.run().await.expect_err("unknown proofs status must prevent publication");
        assert_eq!(
            err.to_string(),
            "failed to check proofs sync status",
            "failure must identify the proofs pre-check"
        );
        assert_eq!(err.root_cause().to_string(), "RPC unavailable", "RPC cause must be retained");
    }

    /// Verifies stale EL heads skip without querying proofs, even when uploads are enabled.
    #[tokio::test]
    async fn stale_el_skips_proofs_check() {
        let mut containers = MockContainerManager::new();
        containers.expect_stop().never();
        containers.expect_start().never();
        containers.expect_is_running().never();
        let mut tip = MockTipChecker::new();
        tip.expect_check_tip()
            .times(1)
            .returning(|_| Ok(TipStatus { block_number: 2_000, at_tip: false }));
        tip.expect_proofs_latest().never();
        let (snapshotter, _tmp) = snapshotter(containers, tip, true, 1_000);
        snapshotter.run().await.expect("stale EL should skip successfully");
    }

    /// Verifies inclusive thresholds, zero lag, and disabled uploads proceed to the lifecycle.
    #[tokio::test]
    async fn eligible_proofs_and_disabled_uploads_proceed() {
        for (upload_proofs, latest, max_lag) in [
            (true, 1_000, 1_000),
            (true, 1_001, 1_000),
            (true, 2_000, 0),
            (true, 2_001, 0),
            (true, 1_990, 10),
            (false, 0, 0),
        ] {
            let mut containers = MockContainerManager::new();
            containers
                .expect_stop()
                .with(mockall::predicate::eq("cl"))
                .times(1)
                .returning(|_| Err(anyhow!("stop sentinel")));
            containers
                .expect_start()
                .with(mockall::predicate::eq("el"))
                .times(1)
                .returning(|_| Ok(()));
            containers
                .expect_start()
                .with(mockall::predicate::eq("cl"))
                .times(1)
                .returning(|_| Ok(()));
            containers.expect_is_running().never();
            let mut tip = MockTipChecker::new();
            tip.expect_check_tip()
                .times(1)
                .returning(|_| Ok(TipStatus { block_number: 2_000, at_tip: true }));
            if upload_proofs {
                tip.expect_proofs_latest().times(1).returning(move || Ok(Some(latest)));
            } else {
                tip.expect_proofs_latest().never();
            }
            let (snapshotter, _tmp) = snapshotter(containers, tip, upload_proofs, max_lag);
            let err = snapshotter
                .run()
                .await
                .expect_err("eligible snapshots must reach the container stop sentinel");
            assert_eq!(
                err.root_cause().to_string(),
                "stop sentinel",
                "proofs eligibility must allow the lifecycle to start"
            );
        }
    }
}
