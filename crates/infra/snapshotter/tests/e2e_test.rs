//! E2E tests for the snapshotter upload flow using `MinIO`.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use base_snapshotter::{ContainerManager, DockerContainerManager, LatestPointer, SnapshotUploader};
use bollard::{
    Docker,
    models::ContainerCreateBody,
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptions,
        StartContainerOptions, StopContainerOptionsBuilder as StopBuilder,
    },
};
use futures::StreamExt;
use serial_test::serial;

mod common;
use common::TestHarness;

struct MockContainerManager {
    running: AtomicBool,
    stop_called: AtomicBool,
    start_called: AtomicBool,
}

impl MockContainerManager {
    const fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            stop_called: AtomicBool::new(false),
            start_called: AtomicBool::new(false),
        }
    }

    fn was_stopped(&self) -> bool {
        self.stop_called.load(Ordering::Relaxed)
    }

    fn was_started(&self) -> bool {
        self.start_called.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ContainerManager for MockContainerManager {
    async fn stop(&self, _container_name: &str) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        self.stop_called.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn start(&self, _container_name: &str) -> Result<()> {
        self.running.store(true, Ordering::Relaxed);
        self.start_called.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn is_running(&self, _container_name: &str) -> Result<bool> {
        Ok(self.running.load(Ordering::Relaxed))
    }
}

/// Builds a realistic fake snapshot matching reth's `SnapshotManifest` format.
///
/// Modeled after the real manifests served at `snapshots-r2.reth.rs`.
fn create_fake_snapshot(dir: &Path, block: u64) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(dir)?;

    let blocks_per_file = 500_000u64;
    let num_chunks = block.div_ceil(blocks_per_file);

    let chunk_sizes: Vec<u64> = (0..num_chunks).map(|i| 1_000_000 + i * 500_000).collect();
    let chunk_decompressed: Vec<u64> = chunk_sizes.iter().map(|s| s * 2).collect();
    let chunk_output_files: Vec<serde_json::Value> = (0..num_chunks)
        .map(|i| {
            let start = i * blocks_per_file;
            let end = (i + 1) * blocks_per_file - 1;
            serde_json::json!([
                {
                    "path": format!("static_files/static_file_headers_{start}_{end}"),
                    "size": chunk_decompressed[i as usize] / 2,
                    "blake3": format!("fake-blake3-headers-{i}")
                },
                {
                    "path": format!("static_files/static_file_headers_{start}_{end}.off"),
                    "size": chunk_decompressed[i as usize] / 2,
                    "blake3": format!("fake-blake3-headers-off-{i}")
                }
            ])
        })
        .collect();

    let chunked_component = |total_blocks| {
        serde_json::json!({
            "blocks_per_file": blocks_per_file,
            "total_blocks": total_blocks,
            "chunk_sizes": chunk_sizes,
            "chunk_decompressed_sizes": chunk_decompressed,
            "chunk_output_files": chunk_output_files
        })
    };

    let manifest = serde_json::json!({
        "block": block,
        "chain_id": 8453,
        "storage_version": 2,
        "timestamp": 1700000000u64,
        "reth_version": "2.1.0 (d58c6e3)",
        "components": {
            "state": {
                "file": "state.tar.zst",
                "size": 152_129_557_628u64,
                "decompressed_size": 304_259_115_256u64,
                "output_files": [
                    {
                        "path": "db/mdbx.dat",
                        "size": 304_259_115_256u64,
                        "blake3": "fake-blake3-mdbx"
                    }
                ]
            },
            "headers": chunked_component(block),
            "transactions": chunked_component(block),
            "transaction_senders": chunked_component(block),
            "receipts": chunked_component(block),
            "account_changesets": chunked_component(block),
            "storage_changesets": chunked_component(block),
            "rocksdb_indices": {
                "file": "rocksdb_indices.tar.zst",
                "size": 226_377_256_076u64,
                "decompressed_size": 452_754_512_152u64,
                "output_files": [
                    {
                        "path": "rocksdb/CURRENT",
                        "size": 16,
                        "blake3": "fake-blake3-rocksdb-current"
                    }
                ]
            }
        }
    });

    let manifest_path = dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    let mut files = vec![manifest_path];

    std::fs::write(dir.join("state.tar.zst"), b"fake-state-archive")?;
    files.push(dir.join("state.tar.zst"));

    std::fs::write(dir.join("rocksdb_indices.tar.zst"), b"fake-rocksdb-archive")?;
    files.push(dir.join("rocksdb_indices.tar.zst"));

    for component in [
        "headers",
        "transactions",
        "transaction_senders",
        "receipts",
        "account_changesets",
        "storage_changesets",
    ] {
        for i in 0..num_chunks {
            let start = i * blocks_per_file;
            let end = (i + 1) * blocks_per_file - 1;
            let filename = format!("{component}-{start}-{end}.tar.zst");
            std::fs::write(dir.join(&filename), format!("fake-{component}-chunk-{i}").as_bytes())?;
            files.push(dir.join(&filename));
        }
    }

    files.sort_unstable();
    Ok(files)
}

#[tokio::test]
#[serial]
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

    let run_prefix = uploader.upload(&output_dir, &files, 1_000_000, 1700000000).await?;

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
    assert_eq!(state_body.as_ref(), b"fake-state-archive", "state archive content mismatch");

    let rocksdb_key = format!("{run_prefix}/rocksdb_indices.tar.zst");
    let rocksdb_obj = harness
        .storage_client
        .get_object()
        .bucket(&harness.bucket_name)
        .key(&rocksdb_key)
        .send()
        .await?;
    let rocksdb_body = rocksdb_obj.body.collect().await?.into_bytes();
    assert_eq!(rocksdb_body.as_ref(), b"fake-rocksdb-archive", "rocksdb archive content mismatch");

    for component in ["headers", "transactions", "receipts"] {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = (chunk_idx + 1) * 500_000 - 1;
            let key = format!("{run_prefix}/{component}-{start}-{end}.tar.zst");
            let obj = harness
                .storage_client
                .get_object()
                .bucket(&harness.bucket_name)
                .key(&key)
                .send()
                .await?;
            let body = obj.body.collect().await?.into_bytes();
            let expected = format!("fake-{component}-chunk-{chunk_idx}");
            assert_eq!(
                body.as_ref(),
                expected.as_bytes(),
                "{component} chunk {chunk_idx} content mismatch"
            );
        }
    }

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
    assert_eq!(manifest["chain_id"], 8453, "manifest chain_id mismatch");
    assert_eq!(manifest["storage_version"], 2, "manifest storage_version mismatch");
    assert!(manifest["reth_version"].is_string(), "manifest should have reth_version");

    let components = manifest["components"].as_object().expect("components should be an object");
    assert_eq!(components.len(), 8, "should have all 8 component types");
    assert!(components.contains_key("state"), "missing state component");
    assert!(components.contains_key("headers"), "missing headers component");
    assert!(components.contains_key("transactions"), "missing transactions component");
    assert!(components.contains_key("transaction_senders"), "missing transaction_senders");
    assert!(components.contains_key("receipts"), "missing receipts component");
    assert!(components.contains_key("account_changesets"), "missing account_changesets");
    assert!(components.contains_key("storage_changesets"), "missing storage_changesets");
    assert!(components.contains_key("rocksdb_indices"), "missing rocksdb_indices");

    Ok(())
}

#[tokio::test]
#[serial]
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

    let run_prefix = uploader.upload(&output_dir, &files, 500_000, 1700000000).await?;

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
#[serial]
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

    let run_prefix = uploader.upload(&output_dir, &files, 100, 1700000000).await?;

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
#[serial]
async fn diff_upload_skips_unchanged_static_file_chunks() -> Result<()> {
    let harness = TestHarness::new().await?;
    let run_prefix = "diff-test/1000000-1700000000";

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "diff-test".to_string(),
    );

    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    // Pre-seed R2 with finalized static file chunks from a previous snapshot run.
    // These represent block ranges that are fully finalized and should NOT be re-uploaded.
    let preexisting: &[(&str, &[u8])] = &[
        ("headers-0-499999.tar.zst", b"finalized-headers-0"),
        ("headers-500000-999999.tar.zst", b"finalized-headers-1"),
        ("transactions-0-499999.tar.zst", b"finalized-txs-0"),
        ("transactions-500000-999999.tar.zst", b"finalized-txs-1"),
        ("receipts-0-499999.tar.zst", b"finalized-receipts-0"),
        ("receipts-500000-999999.tar.zst", b"finalized-receipts-1"),
        ("account_changesets-0-499999.tar.zst", b"finalized-acc-cs-0"),
        ("storage_changesets-0-499999.tar.zst", b"finalized-stor-cs-0"),
    ];

    for (name, data) in preexisting {
        let key = format!("{run_prefix}/{name}");
        s3.put_object()
            .bucket(bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.to_vec()))
            .send()
            .await?;
    }

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    std::fs::create_dir_all(&output_dir)?;

    let manifest = serde_json::json!({
        "block": 1_000_000,
        "chain_id": 8453,
        "storage_version": 2,
        "reth_version": "2.1.0 (d58c6e3)",
        "components": {
            "state": {"file": "state.tar.zst", "size": 100, "decompressed_size": 200, "output_files": []},
            "rocksdb_indices": {"file": "rocksdb_indices.tar.zst", "size": 50, "decompressed_size": 100, "output_files": []},
            "headers": {"blocks_per_file": 500000, "total_blocks": 1000000, "chunk_sizes": [20, 20], "chunk_decompressed_sizes": [], "chunk_output_files": []},
            "transactions": {"blocks_per_file": 500000, "total_blocks": 1000000, "chunk_sizes": [30, 30], "chunk_decompressed_sizes": [], "chunk_output_files": []},
            "receipts": {"blocks_per_file": 500000, "total_blocks": 1000000, "chunk_sizes": [15, 15], "chunk_decompressed_sizes": [], "chunk_output_files": []},
            "account_changesets": {"blocks_per_file": 500000, "total_blocks": 1000000, "chunk_sizes": [18, 18], "chunk_decompressed_sizes": [], "chunk_output_files": []},
            "storage_changesets": {"blocks_per_file": 500000, "total_blocks": 1000000, "chunk_sizes": [22, 22], "chunk_decompressed_sizes": [], "chunk_output_files": []}
        }
    });
    std::fs::write(output_dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;

    // AlwaysUpload: mdbx state + rocksdb (always re-uploaded, content changes every snapshot)
    std::fs::write(output_dir.join("state.tar.zst"), b"new-mdbx-state-data")?;
    std::fs::write(output_dir.join("rocksdb_indices.tar.zst"), b"new-rocksdb-data")?;

    // DiffBySize: finalized chunks with SAME size as remote → should be SKIPPED
    for (name, data) in preexisting {
        std::fs::write(output_dir.join(name), *data)?;
    }

    // DiffBySize: tip chunks that are NEW (don't exist in remote) → should be UPLOADED
    std::fs::write(output_dir.join("headers-1000000-1499999.tar.zst"), b"new-tip-headers")?;
    std::fs::write(output_dir.join("transactions-1000000-1499999.tar.zst"), b"new-tip-txs")?;
    std::fs::write(output_dir.join("receipts-1000000-1499999.tar.zst"), b"new-tip-receipts")?;
    std::fs::write(output_dir.join("account_changesets-500000-999999.tar.zst"), b"new-tip-acc-cs")?;
    std::fs::write(
        output_dir.join("storage_changesets-500000-999999.tar.zst"),
        b"new-tip-stor-cs",
    )?;

    let mut files: Vec<PathBuf> = std::fs::read_dir(&output_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort_unstable();

    uploader.upload(&output_dir, &files, 1_000_000, 1_700_000_000).await?;

    // Verify AlwaysUpload: mdbx state re-uploaded
    let state_body = get_object_bytes(s3, bucket, &format!("{run_prefix}/state.tar.zst")).await?;
    assert_eq!(state_body, b"new-mdbx-state-data", "mdbx state should always be re-uploaded");

    // Verify AlwaysUpload: rocksdb re-uploaded
    let rocksdb_body =
        get_object_bytes(s3, bucket, &format!("{run_prefix}/rocksdb_indices.tar.zst")).await?;
    assert_eq!(rocksdb_body, b"new-rocksdb-data", "rocksdb should always be re-uploaded");

    // Verify DiffBySize SKIPPED: finalized chunks retain original content
    for (name, original_data) in preexisting {
        let body = get_object_bytes(s3, bucket, &format!("{run_prefix}/{name}")).await?;
        assert_eq!(
            body.as_slice(),
            *original_data,
            "finalized chunk {name} should be skipped (content unchanged)"
        );
    }

    // Verify DiffBySize UPLOADED: new tip chunks exist
    let tip_checks: &[(&str, &[u8])] = &[
        ("headers-1000000-1499999.tar.zst", b"new-tip-headers"),
        ("transactions-1000000-1499999.tar.zst", b"new-tip-txs"),
        ("receipts-1000000-1499999.tar.zst", b"new-tip-receipts"),
        ("account_changesets-500000-999999.tar.zst", b"new-tip-acc-cs"),
        ("storage_changesets-500000-999999.tar.zst", b"new-tip-stor-cs"),
    ];
    for (name, expected) in tip_checks {
        let body = get_object_bytes(s3, bucket, &format!("{run_prefix}/{name}")).await?;
        assert_eq!(body.as_slice(), *expected, "tip chunk {name} should be uploaded");
    }

    // Verify manifest always uploaded
    let manifest_body =
        get_object_bytes(s3, bucket, &format!("{run_prefix}/manifest.json")).await?;
    let parsed: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    assert_eq!(parsed["block"], 1_000_000, "manifest should be uploaded with correct block");

    Ok(())
}

#[tokio::test]
#[serial]
async fn always_upload_overwrites_existing_state_and_rocksdb() -> Result<()> {
    let harness = TestHarness::new().await?;
    let run_prefix = "overwrite-test/2000000-1700000000";

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "overwrite-test".to_string(),
    );

    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    // Pre-seed R2 with stale state and rocksdb from a previous snapshot
    let stale: &[(&str, &[u8])] = &[
        ("state.tar.zst", b"old-mdbx-snapshot-from-yesterday"),
        ("rocksdb_indices.tar.zst", b"old-rocksdb-from-yesterday"),
    ];
    for (name, data) in stale {
        let key = format!("{run_prefix}/{name}");
        s3.put_object()
            .bucket(bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.to_vec()))
            .send()
            .await?;
    }

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    std::fs::create_dir_all(&output_dir)?;

    let manifest = serde_json::json!({
        "block": 2_000_000, "chain_id": 8453, "storage_version": 2,
        "components": {
            "state": {"file": "state.tar.zst", "size": 100, "decompressed_size": 200, "output_files": []},
            "rocksdb_indices": {"file": "rocksdb_indices.tar.zst", "size": 50, "decompressed_size": 100, "output_files": []}
        }
    });
    std::fs::write(output_dir.join("manifest.json"), serde_json::to_string(&manifest)?)?;
    std::fs::write(output_dir.join("state.tar.zst"), b"fresh-mdbx-state")?;
    std::fs::write(output_dir.join("rocksdb_indices.tar.zst"), b"fresh-rocksdb")?;

    let files = vec![
        output_dir.join("manifest.json"),
        output_dir.join("rocksdb_indices.tar.zst"),
        output_dir.join("state.tar.zst"),
    ];

    uploader.upload(&output_dir, &files, 2_000_000, 1_700_000_000).await?;

    // Verify state was overwritten, not skipped
    let state_body = get_object_bytes(s3, bucket, &format!("{run_prefix}/state.tar.zst")).await?;
    assert_eq!(
        state_body, b"fresh-mdbx-state",
        "state.tar.zst should be overwritten even though it existed in R2"
    );

    // Verify rocksdb was overwritten, not skipped
    let rocksdb_body =
        get_object_bytes(s3, bucket, &format!("{run_prefix}/rocksdb_indices.tar.zst")).await?;
    assert_eq!(
        rocksdb_body, b"fresh-rocksdb",
        "rocksdb_indices.tar.zst should be overwritten even though it existed in R2"
    );

    Ok(())
}

async fn get_object_bytes(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
    let resp = client.get_object().bucket(bucket).key(key).send().await?;
    let bytes = resp.body.collect().await?.into_bytes();
    Ok(bytes.to_vec())
}

#[tokio::test]
#[serial]
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
#[serial]
async fn orchestrator_always_restarts_on_failure() -> Result<()> {
    let harness = TestHarness::new().await?;
    let manager = std::sync::Arc::new(MockContainerManager::new());
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "test".to_string(),
    );

    let tmp = tempfile::tempdir()?;

    let config = base_snapshotter::SnapshotterConfig {
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
        s3_config_type: base_snapshotter::S3ConfigType::Aws,
        s3_endpoint: None,
        s3_region: "us-east-1".to_string(),
        s3_access_key_id: None,
        s3_secret_access_key: None,
    };

    let snapshotter =
        base_snapshotter::Snapshotter::new(std::sync::Arc::clone(&manager), uploader, config);

    let result = snapshotter.run().await;
    assert!(result.is_err(), "should fail because source_datadir doesn't exist");
    assert!(manager.was_stopped(), "container should have been stopped");
    assert!(manager.was_started(), "container should always be restarted even on failure");

    Ok(())
}

/// E2E test: spins up a real Docker container, stops it via bollard,
/// creates fake snapshot artifacts, uploads to `MinIO`, then restarts the container.
/// Verifies the container is running again after the full lifecycle.
#[tokio::test]
#[serial]
async fn e2e_stop_upload_restart_real_container() -> Result<()> {
    let harness = TestHarness::new().await?;

    let docker = Docker::connect_with_socket_defaults()
        .expect("failed to connect to Docker — is Docker running?");

    let pull_opts = CreateImageOptionsBuilder::new().from_image("alpine").tag("latest").build();
    docker.create_image(Some(pull_opts), None, None).collect::<Vec<_>>().await;

    let container_name = format!("snappy-e2e-{}", std::process::id());
    let body = ContainerCreateBody {
        image: Some("alpine:latest".to_string()),
        cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
        ..Default::default()
    };

    let create_opts = CreateContainerOptionsBuilder::new().name(&container_name).build();
    docker.create_container(Some(create_opts), body).await?;

    docker.start_container(&container_name, None::<StartContainerOptions>).await?;

    let container_manager = DockerContainerManager::new("/var/run/docker.sock")?;

    assert!(
        container_manager.is_running(&container_name).await?,
        "container should be running before snappy"
    );

    container_manager.stop(&container_name).await?;
    assert!(
        !container_manager.is_running(&container_name).await?,
        "container should be stopped after stop()"
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 1_000_000)?;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "e2e-test".to_string(),
    );
    let run_prefix = uploader.upload(&output_dir, &files, 1_000_000, 1700000000).await?;

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
    assert_eq!(manifest["block"], 1_000_000, "uploaded manifest should have correct block");

    container_manager.start(&container_name).await?;
    assert!(
        container_manager.is_running(&container_name).await?,
        "container should be running after restart"
    );

    docker.stop_container(&container_name, Some(StopBuilder::new().t(5).build())).await.ok();
    docker.remove_container(&container_name, None::<RemoveContainerOptions>).await.ok();

    Ok(())
}
