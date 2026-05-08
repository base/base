//! E2E tests for the snapshotter upload flow using `MinIO`.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use base_snapshotter::{ContainerManager, DockerContainerManager, SnapshotUploader};
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
fn create_fake_snapshot(dir: &Path, block: u64) -> Result<Vec<PathBuf>> {
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
                "output_files": [{"path": "db/mdbx.dat", "size": 304_259_115_256u64, "blake3": "fake-blake3-mdbx"}]
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
                "output_files": [{"path": "rocksdb/CURRENT", "size": 16, "blake3": "fake-blake3-rocksdb-current"}]
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

    let upload_prefix = uploader.upload(&output_dir, &files, 1_700_000_000).await?;
    assert_eq!(upload_prefix, "mainnet/1700000000", "run prefix should be date-based");

    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    // Verify always-upload files go to {prefix}/{date}/
    let state_body = get_object_bytes(s3, bucket, "mainnet/1700000000/state.tar.zst").await?;
    assert_eq!(state_body, b"fake-state-archive", "state should be in date dir");

    let rocksdb_body =
        get_object_bytes(s3, bucket, "mainnet/1700000000/rocksdb_indices.tar.zst").await?;
    assert_eq!(rocksdb_body, b"fake-rocksdb-archive", "rocksdb should be in date dir");

    let manifest_body = get_object_bytes(s3, bucket, "mainnet/1700000000/manifest.json").await?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    assert_eq!(manifest["block"], 1_000_000, "manifest block mismatch");
    assert_eq!(manifest["chain_id"], 8453, "manifest chain_id mismatch");

    let components = manifest["components"].as_object().expect("components should be an object");
    assert_eq!(components.len(), 8, "should have all 8 component types");

    // Verify static file chunks go to {prefix}/static_files/
    for component in ["headers", "transactions", "receipts"] {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = (chunk_idx + 1) * 500_000 - 1;
            let key = format!("mainnet/static_files/{component}-{start}-{end}.tar.zst");
            let body = get_object_bytes(s3, bucket, &key).await?;
            let expected = format!("fake-{component}-chunk-{chunk_idx}");
            assert_eq!(
                body,
                expected.as_bytes(),
                "{component} chunk {chunk_idx} should be in static_files/"
            );
        }
    }

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

    let upload_prefix = uploader.upload(&output_dir, &files, 1_700_000_000).await?;
    assert_eq!(upload_prefix, "1700000000", "empty prefix should produce bare date");

    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let state_body = get_object_bytes(s3, bucket, "1700000000/state.tar.zst").await?;
    assert_eq!(state_body, b"fake-state-archive", "state should be in date dir");

    let rocksdb_body = get_object_bytes(s3, bucket, "1700000000/rocksdb_indices.tar.zst").await?;
    assert_eq!(rocksdb_body, b"fake-rocksdb-archive", "rocksdb should be in date dir");

    let manifest_body = get_object_bytes(s3, bucket, "1700000000/manifest.json").await?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    assert_eq!(manifest["block"], 100, "manifest should be in date dir");

    let headers_body =
        get_object_bytes(s3, bucket, "static_files/headers-0-499999.tar.zst").await?;
    assert_eq!(headers_body, b"fake-headers-chunk-0", "headers chunk 0 should be in static_files/");

    Ok(())
}

#[tokio::test]
#[serial]
async fn diff_upload_skips_unchanged_static_file_chunks() -> Result<()> {
    let harness = TestHarness::new().await?;
    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "diff-test".to_string(),
    );

    // Pre-seed static_files/ with finalized chunks from a previous run
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
        let key = format!("diff-test/static_files/{name}");
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

    let manifest = serde_json::json!({"block": 1_000_000, "chain_id": 8453, "storage_version": 2});
    std::fs::write(output_dir.join("manifest.json"), serde_json::to_string(&manifest)?)?;

    // AlwaysUpload: mdbx + rocksdb
    std::fs::write(output_dir.join("state.tar.zst"), b"new-mdbx-state-data")?;
    std::fs::write(output_dir.join("rocksdb_indices.tar.zst"), b"new-rocksdb-data")?;

    // DiffBySize: finalized chunks with SAME size → should be SKIPPED
    for &(name, data) in preexisting {
        std::fs::write(output_dir.join(name), data)?;
    }

    // DiffBySize: new tip chunks → should be UPLOADED
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

    let upload_prefix = uploader.upload(&output_dir, &files, 1_700_000_000).await?;
    assert_eq!(upload_prefix, "diff-test/1700000000");

    // Verify AlwaysUpload: mdbx + rocksdb in date dir
    let state_body = get_object_bytes(s3, bucket, "diff-test/1700000000/state.tar.zst").await?;
    assert_eq!(state_body, b"new-mdbx-state-data", "mdbx should be in date dir");

    let rocksdb_body =
        get_object_bytes(s3, bucket, "diff-test/1700000000/rocksdb_indices.tar.zst").await?;
    assert_eq!(rocksdb_body, b"new-rocksdb-data", "rocksdb should be in date dir");

    // Verify DiffBySize SKIPPED: finalized chunks retain original content in static_files/
    for (name, original_data) in preexisting {
        let body = get_object_bytes(s3, bucket, &format!("diff-test/static_files/{name}")).await?;
        assert_eq!(body.as_slice(), *original_data, "finalized chunk {name} should be unchanged");
    }

    // Verify DiffBySize UPLOADED: new tip chunks in static_files/
    let tip_checks: &[(&str, &[u8])] = &[
        ("headers-1000000-1499999.tar.zst", b"new-tip-headers"),
        ("transactions-1000000-1499999.tar.zst", b"new-tip-txs"),
        ("receipts-1000000-1499999.tar.zst", b"new-tip-receipts"),
        ("account_changesets-500000-999999.tar.zst", b"new-tip-acc-cs"),
        ("storage_changesets-500000-999999.tar.zst", b"new-tip-stor-cs"),
    ];
    for (name, expected) in tip_checks {
        let body = get_object_bytes(s3, bucket, &format!("diff-test/static_files/{name}")).await?;
        assert_eq!(body.as_slice(), *expected, "tip chunk {name} should be in static_files/");
    }

    // Verify manifest in date dir
    let manifest_body = get_object_bytes(s3, bucket, "diff-test/1700000000/manifest.json").await?;
    let parsed: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    assert_eq!(parsed["block"], 1_000_000, "manifest should be in date dir");

    Ok(())
}

#[tokio::test]
#[serial]
async fn always_upload_overwrites_existing_state_and_rocksdb() -> Result<()> {
    let harness = TestHarness::new().await?;
    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "overwrite-test".to_string(),
    );

    // Simulate a previous run's date dir with old state + rocksdb
    let prev_files: &[(&str, &[u8])] = &[
        ("state.tar.zst", b"old-mdbx-from-yesterday"),
        ("rocksdb_indices.tar.zst", b"old-rocksdb-from-yesterday"),
        ("manifest.json", b"{\"block\":1500000}"),
    ];
    for (name, data) in prev_files {
        let key = format!("overwrite-test/1699000000/{name}");
        s3.put_object()
            .bucket(bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.to_vec()))
            .send()
            .await?;
    }

    // New snapshot
    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    std::fs::create_dir_all(&output_dir)?;

    let manifest = serde_json::json!({"block": 2_000_000, "chain_id": 8453, "storage_version": 2});
    std::fs::write(output_dir.join("manifest.json"), serde_json::to_string(&manifest)?)?;
    std::fs::write(output_dir.join("state.tar.zst"), b"fresh-mdbx-state")?;
    std::fs::write(output_dir.join("rocksdb_indices.tar.zst"), b"fresh-rocksdb")?;

    let files = vec![
        output_dir.join("manifest.json"),
        output_dir.join("rocksdb_indices.tar.zst"),
        output_dir.join("state.tar.zst"),
    ];

    let upload_prefix = uploader.upload(&output_dir, &files, 1_700_000_000).await?;
    assert_eq!(upload_prefix, "overwrite-test/1700000000");

    // Verify new state in new date dir
    let state_body =
        get_object_bytes(s3, bucket, "overwrite-test/1700000000/state.tar.zst").await?;
    assert_eq!(state_body, b"fresh-mdbx-state", "state should be in new date dir");

    // Verify new rocksdb in new date dir
    let rocksdb_body =
        get_object_bytes(s3, bucket, "overwrite-test/1700000000/rocksdb_indices.tar.zst").await?;
    assert_eq!(rocksdb_body, b"fresh-rocksdb", "rocksdb should be in new date dir");

    // Verify previous run's files are untouched
    let old_state = get_object_bytes(s3, bucket, "overwrite-test/1699000000/state.tar.zst").await?;
    assert_eq!(
        old_state.as_slice(),
        b"old-mdbx-from-yesterday",
        "previous run should be untouched"
    );

    Ok(())
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

    let container_name = format!("snapshotter-e2e-{}", std::process::id());
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
        "container should be running before snapshotter"
    );

    container_manager.stop(&container_name).await?;
    assert!(!container_manager.is_running(&container_name).await?, "should be stopped");

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 1_000_000)?;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "e2e-test".to_string(),
    );
    let upload_prefix = uploader.upload(&output_dir, &files, 1_700_000_000).await?;

    let manifest_body = get_object_bytes(
        &harness.storage_client,
        &harness.bucket_name,
        &format!("{upload_prefix}/manifest.json"),
    )
    .await?;
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

async fn get_object_bytes(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
    let resp = client.get_object().bucket(bucket).key(key).send().await?;
    let bytes = resp.body.collect().await?.into_bytes();
    Ok(bytes.to_vec())
}
