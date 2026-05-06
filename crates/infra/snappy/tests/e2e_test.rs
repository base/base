//! E2E tests for the snapshotter upload flow using `MinIO`.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use base_snappy::{ContainerManager, LatestPointer, SnapshotUploader};

mod common;
use common::TestHarness;

struct MockContainerManager {
    stop_called: std::sync::atomic::AtomicBool,
    start_called: std::sync::atomic::AtomicBool,
}

impl MockContainerManager {
    const fn new() -> Self {
        Self {
            stop_called: std::sync::atomic::AtomicBool::new(false),
            start_called: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn was_stopped(&self) -> bool {
        self.stop_called.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn was_started(&self) -> bool {
        self.start_called.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl ContainerManager for MockContainerManager {
    async fn stop(&self, _container_name: &str) -> Result<()> {
        self.stop_called.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    async fn start(&self, _container_name: &str) -> Result<()> {
        self.start_called.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    async fn is_running(&self, _container_name: &str) -> Result<bool> {
        Ok(!self.stop_called.load(std::sync::atomic::Ordering::Relaxed))
    }
}

fn create_fake_snapshot(dir: &Path, block: u64) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(dir)?;

    let manifest = serde_json::json!({
        "block": block,
        "chain_id": 8453,
        "storage_version": 2,
        "timestamp": 1700000000u64,
        "components": {
            "state": {
                "file": "state.tar.zst",
                "size": 100,
                "decompressed_size": 200,
                "output_files": []
            }
        }
    });

    let manifest_path = dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    let state_path = dir.join("state.tar.zst");
    std::fs::write(&state_path, b"fake-state-archive-data")?;

    let headers_path = dir.join("headers-0-499999.tar.zst");
    std::fs::write(&headers_path, b"fake-headers-archive-data")?;

    let mut files = vec![headers_path, manifest_path, state_path];
    files.sort_unstable();
    Ok(files)
}

#[tokio::test]
async fn upload_artifacts_to_minio() -> Result<()> {
    let harness = TestHarness::new().await?;
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "mainnet".to_string(),
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 1_000_000)?;

    let run_prefix = uploader.upload(&output_dir, &files, 1_000_000).await?;

    assert!(
        run_prefix.starts_with("mainnet/1000000-"),
        "run_prefix should start with mainnet/1000000-, got: {run_prefix}"
    );

    let state_key = format!("{run_prefix}/state.tar.zst");
    let state_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key(&state_key)
        .send()
        .await?;
    let state_body = state_obj.body.collect().await?.into_bytes();
    assert_eq!(state_body.as_ref(), b"fake-state-archive-data", "state archive content mismatch");

    let headers_key = format!("{run_prefix}/headers-0-499999.tar.zst");
    let headers_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key(&headers_key)
        .send()
        .await?;
    let headers_body = headers_obj.body.collect().await?.into_bytes();
    assert_eq!(
        headers_body.as_ref(),
        b"fake-headers-archive-data",
        "headers archive content mismatch"
    );

    let manifest_key = format!("{run_prefix}/manifest.json");
    let manifest_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key(&manifest_key)
        .send()
        .await?;
    let manifest_body = manifest_obj.body.collect().await?.into_bytes();
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    assert_eq!(manifest["block"], 1_000_000, "manifest block number mismatch");

    Ok(())
}

#[tokio::test]
async fn latest_pointer_updated_after_upload() -> Result<()> {
    let harness = TestHarness::new().await?;
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "sepolia".to_string(),
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 500_000)?;

    let run_prefix = uploader.upload(&output_dir, &files, 500_000).await?;

    let latest_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key("sepolia/latest.json")
        .send()
        .await?;
    let latest_body = latest_obj.body.collect().await?.into_bytes();
    let pointer: LatestPointer = serde_json::from_slice(&latest_body)?;

    assert_eq!(pointer.block, 500_000, "latest pointer block mismatch");
    assert_eq!(pointer.prefix, run_prefix, "latest pointer prefix mismatch");
    assert!(pointer.timestamp > 0, "latest pointer should have a non-zero timestamp");

    Ok(())
}

#[tokio::test]
async fn upload_with_empty_prefix() -> Result<()> {
    let harness = TestHarness::new().await?;
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        String::new(),
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 100)?;

    let run_prefix = uploader.upload(&output_dir, &files, 100).await?;

    assert!(
        run_prefix.starts_with("100-"),
        "with empty prefix, run_prefix should start with block number, got: {run_prefix}"
    );

    let latest_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key("latest.json")
        .send()
        .await?;
    let latest_body = latest_obj.body.collect().await?.into_bytes();
    let pointer: LatestPointer = serde_json::from_slice(&latest_body)?;
    assert_eq!(pointer.block, 100, "latest pointer block should be 100");

    Ok(())
}

#[tokio::test]
async fn mock_container_manager_tracks_calls() -> Result<()> {
    let manager = MockContainerManager::new();

    assert!(!manager.was_stopped(), "should not be stopped initially");
    assert!(!manager.was_started(), "should not be started initially");

    manager.stop("test-container").await?;
    assert!(manager.was_stopped(), "should be stopped after stop()");

    manager.start("test-container").await?;
    assert!(manager.was_started(), "should be started after start()");

    Ok(())
}

#[tokio::test]
async fn orchestrator_always_restarts_container() -> Result<()> {
    let harness = TestHarness::new().await?;
    let manager = MockContainerManager::new();
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "test".to_string(),
    );

    let tmp = tempfile::tempdir()?;

    let config = base_snappy::SnapshotterConfig {
        container_name: "fake-el".to_string(),
        source_datadir: tmp.path().join("nonexistent-datadir"),
        output_dir: tmp.path().join("output"),
        bucket: harness.bucket_name.clone(),
        prefix: "test".to_string(),
        chain_id: 8453,
        block: Some(100),
        blocks_per_file: Some(500_000),
        snapshot_threads: None,
        docker_socket: "/var/run/docker.sock".to_string(),
        endpoint_url: None,
        region: "auto".to_string(),
    };

    let snapshotter = base_snappy::Snapshotter::new(manager, uploader, config);

    let result = snapshotter.run().await;
    assert!(result.is_err(), "should fail because source_datadir doesn't exist");

    Ok(())
}
