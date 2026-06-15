//! E2E tests for the snapshotter upload flow using `MinIO`.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use base_snapshotter::{
    ChunkedArchive, ComponentManifest, ContainerManager, DockerContainerManager,
    OutputFileChecksum, SnapshotGenerator, SnapshotManifest, SnapshotUploader,
};
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

/// Parses a `manifest.json` written into `output_dir` by `create_fake_snapshot`
/// or `SnapshotGenerator::generate_manifest`.
fn parse_local_manifest(output_dir: &Path) -> Result<SnapshotManifest> {
    let bytes = std::fs::read(output_dir.join("manifest.json"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Minimal `SnapshotManifest` with no components — for tests that don't
/// upload any `DiffByHash` chunks and therefore don't need real hashes.
const fn empty_manifest(block: u64) -> SnapshotManifest {
    SnapshotManifest {
        block,
        chain_id: 8453,
        storage_version: 2,
        timestamp: 0,
        base_url: None,
        reth_version: None,
        components: BTreeMap::new(),
    }
}

/// Builds a `SnapshotManifest` whose chunked components carry per-chunk
/// `OutputFileChecksum` entries derived from `hash_seed`. Two manifests built
/// with the same seed will produce identical BLAKE3s for every chunk (→ skip).
/// Different seeds → mismatch (→ re-upload).
fn manifest_with_seeded_hashes(
    block: u64,
    blocks_per_file: u64,
    components: &[&str],
    hash_seed: &str,
) -> SnapshotManifest {
    let num_chunks = block.div_ceil(blocks_per_file);
    let mut comps = BTreeMap::new();
    for &component in components {
        let chunk_output_files: Vec<Vec<OutputFileChecksum>> = (0..num_chunks)
            .map(|i| {
                let start = i * blocks_per_file;
                let end = start + blocks_per_file - 1;
                vec![
                    OutputFileChecksum {
                        path: format!("static_files/static_file_{component}_{start}_{end}"),
                        size: 100,
                        blake3: format!("{hash_seed}-{component}-{i}-main"),
                    },
                    OutputFileChecksum {
                        path: format!("static_files/static_file_{component}_{start}_{end}.off"),
                        size: 100,
                        blake3: format!("{hash_seed}-{component}-{i}-off"),
                    },
                ]
            })
            .collect();
        comps.insert(
            component.to_string(),
            ComponentManifest::Chunked(ChunkedArchive {
                blocks_per_file,
                total_blocks: block,
                chunk_sizes: vec![100u64; num_chunks as usize],
                chunk_decompressed_sizes: vec![200u64; num_chunks as usize],
                chunk_output_files,
            }),
        );
    }
    SnapshotManifest {
        block,
        chain_id: 8453,
        storage_version: 2,
        timestamp: 0,
        base_url: None,
        reth_version: None,
        components: comps,
    }
}

#[tokio::test]
#[serial]
async fn upload_artifacts_to_minio() -> Result<()> {
    let harness = TestHarness::new().await?;
    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "mainnet".to_string(),
        Some("https://snapshots.example.com".to_string()),
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 1_000_000)?;
    let local_manifest = parse_local_manifest(&output_dir)?;

    let upload_prefix =
        uploader.upload(&output_dir, &files, 1_700_000_000, &local_manifest, None).await?;
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
    assert_eq!(
        manifest["base_url"], "https://snapshots.example.com/mainnet/static_files",
        "manifest should point chunk downloads at top-level static_files"
    );
    assert_eq!(
        manifest["components"]["state"]["file"], "../1700000000/state.tar.zst",
        "manifest should point state back to the dated run dir"
    );

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
        Some("https://snapshots.example.com".to_string()),
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = create_fake_snapshot(&output_dir, 100)?;
    let local_manifest = parse_local_manifest(&output_dir)?;

    let upload_prefix =
        uploader.upload(&output_dir, &files, 1_700_000_000, &local_manifest, None).await?;
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
    assert_eq!(
        manifest["base_url"], "https://snapshots.example.com/static_files",
        "manifest should point chunk downloads at top-level static_files"
    );
    assert_eq!(
        manifest["components"]["state"]["file"], "../1700000000/state.tar.zst",
        "manifest should point state back to the dated run dir"
    );

    let headers_body =
        get_object_bytes(s3, bucket, "static_files/headers-0-499999.tar.zst").await?;
    assert_eq!(headers_body, b"fake-headers-chunk-0", "headers chunk 0 should be in static_files/");

    Ok(())
}

/// Helper: writes a `manifest.json` and a complete set of fake artifacts to
/// `output_dir`. The chunked-component sections of the manifest carry BLAKE3
/// hashes derived from `hash_seed`, so two runs sharing a seed will produce
/// matching hashes and trigger the `DiffByHash` skip path.
fn seeded_snapshot(
    output_dir: &Path,
    block: u64,
    blocks_per_file: u64,
    components: &[&str],
    hash_seed: &str,
    chunk_content: &[u8],
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(output_dir)?;
    let manifest = manifest_with_seeded_hashes(block, blocks_per_file, components, hash_seed);
    std::fs::write(output_dir.join("manifest.json"), serde_json::to_string_pretty(&manifest)?)?;
    std::fs::write(output_dir.join("state.tar.zst"), b"state-data")?;
    std::fs::write(output_dir.join("rocksdb_indices.tar.zst"), b"rocksdb-data")?;

    let num_chunks = block.div_ceil(blocks_per_file);
    for &component in components {
        for i in 0..num_chunks {
            let start = i * blocks_per_file;
            let end = start + blocks_per_file - 1;
            std::fs::write(
                output_dir.join(format!("{component}-{start}-{end}.tar.zst")),
                chunk_content,
            )?;
        }
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(output_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort_unstable();
    Ok(files)
}

const DIFF_TEST_COMPONENTS: &[&str] = &[
    "headers",
    "transactions",
    "receipts",
    "account_changesets",
    "storage_changesets",
    "transaction_senders",
];

#[tokio::test]
#[serial]
async fn diff_upload_skips_chunks_when_blake3_matches() -> Result<()> {
    let harness = TestHarness::new().await?;
    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "diff-match".to_string(),
        None,
    );

    // Pre-seed static_files/ with the prior run's chunks and a previous manifest.json
    // at {prefix}/1699000000/manifest.json. Both use the same hash seed.
    let prev_manifest = manifest_with_seeded_hashes(1_000_000, 500_000, DIFF_TEST_COMPONENTS, "v1");
    s3.put_object()
        .bucket(bucket)
        .key("diff-match/1699000000/manifest.json")
        .body(serde_json::to_vec(&prev_manifest)?.into())
        .send()
        .await?;

    for &component in DIFF_TEST_COMPONENTS {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = start + 499_999;
            let key = format!("diff-match/static_files/{component}-{start}-{end}.tar.zst");
            s3.put_object()
                .bucket(bucket)
                .key(&key)
                .body(aws_sdk_s3::primitives::ByteStream::from(b"old-bytes".to_vec()))
                .send()
                .await?;
        }
    }

    // Generate a new run that produces the SAME hashes (same seed) but with
    // different on-disk byte content — proves the comparison is hash-based,
    // not byte-based.
    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files =
        seeded_snapshot(&output_dir, 1_000_000, 500_000, DIFF_TEST_COMPONENTS, "v1", b"new-bytes")?;
    let local_manifest = parse_local_manifest(&output_dir)?;
    let remote_manifest = uploader.fetch_previous_manifest().await?;
    assert!(remote_manifest.is_some(), "should find the pre-seeded previous manifest");

    uploader
        .upload(&output_dir, &files, 1_700_000_000, &local_manifest, remote_manifest.as_ref())
        .await?;

    // Verify: pre-seeded chunks were NOT overwritten (skipped due to blake3 match).
    for &component in DIFF_TEST_COMPONENTS {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = start + 499_999;
            let key = format!("diff-match/static_files/{component}-{start}-{end}.tar.zst");
            let body = get_object_bytes(s3, bucket, &key).await?;
            assert_eq!(
                body.as_slice(),
                b"old-bytes",
                "chunk {component} {chunk_idx} should have been skipped (blake3 match)"
            );
        }
    }

    let manifest_body = get_object_bytes(s3, bucket, "diff-match/1700000000/manifest.json").await?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_body)?;
    let chunk_output_files = manifest["components"]["headers"]["chunk_output_files"]
        .as_array()
        .expect("headers chunk_output_files should be an array");
    assert!(
        chunk_output_files
            .iter()
            .all(|entry| entry.as_array().is_some_and(|files| !files.is_empty())),
        "download manifest should preserve output-file metadata for skipped chunks"
    );

    Ok(())
}

#[tokio::test]
#[serial]
async fn diff_upload_reuploads_on_blake3_mismatch_even_when_size_matches() -> Result<()> {
    // This is the core correctness case the PR exists to fix: a chunk whose
    // contents changed in a way that preserves its archive size. Size-based
    // diff would skip; hash-based diff must re-upload.
    let harness = TestHarness::new().await?;
    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "diff-mismatch".to_string(),
        None,
    );

    // Previous run published hashes with seed "v1".
    let prev_manifest = manifest_with_seeded_hashes(1_000_000, 500_000, DIFF_TEST_COMPONENTS, "v1");
    s3.put_object()
        .bucket(bucket)
        .key("diff-mismatch/1699000000/manifest.json")
        .body(serde_json::to_vec(&prev_manifest)?.into())
        .send()
        .await?;

    for &component in DIFF_TEST_COMPONENTS {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = start + 499_999;
            let key = format!("diff-mismatch/static_files/{component}-{start}-{end}.tar.zst");
            s3.put_object()
                .bucket(bucket)
                .key(&key)
                .body(aws_sdk_s3::primitives::ByteStream::from(b"corrupted-bytes-x".to_vec()))
                .send()
                .await?;
        }
    }

    // New run reports DIFFERENT hashes (seed "v2") for the same chunk filenames.
    // Local chunk bytes are the same length as remote, so size comparison would
    // have skipped — but blake3 comparison should force a re-upload.
    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = seeded_snapshot(
        &output_dir,
        1_000_000,
        500_000,
        DIFF_TEST_COMPONENTS,
        "v2",
        b"healthy-bytes-yyy",
    )?;
    assert_eq!(
        b"healthy-bytes-yyy".len(),
        b"corrupted-bytes-x".len(),
        "test fixture must hold size constant"
    );
    let local_manifest = parse_local_manifest(&output_dir)?;
    let remote_manifest = uploader.fetch_previous_manifest().await?;

    uploader
        .upload(&output_dir, &files, 1_700_000_000, &local_manifest, remote_manifest.as_ref())
        .await?;

    // Verify every chunk was re-uploaded with the fresh bytes.
    for &component in DIFF_TEST_COMPONENTS {
        for chunk_idx in 0..2u64 {
            let start = chunk_idx * 500_000;
            let end = start + 499_999;
            let key = format!("diff-mismatch/static_files/{component}-{start}-{end}.tar.zst");
            let body = get_object_bytes(s3, bucket, &key).await?;
            assert_eq!(
                body.as_slice(),
                b"healthy-bytes-yyy",
                "chunk {component} {chunk_idx} should have been re-uploaded despite size match"
            );
        }
    }

    Ok(())
}

#[tokio::test]
#[serial]
async fn diff_upload_uploads_everything_on_first_run() -> Result<()> {
    let harness = TestHarness::new().await?;
    let s3 = &harness.storage_client;
    let bucket = &harness.bucket_name;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "first-run".to_string(),
        None,
    );

    let tmp = tempfile::tempdir()?;
    let output_dir = tmp.path().join("output");
    let files = seeded_snapshot(&output_dir, 500_000, 500_000, &["headers"], "v1", b"fresh-chunk")?;
    let local_manifest = parse_local_manifest(&output_dir)?;
    let remote_manifest = uploader.fetch_previous_manifest().await?;
    assert!(remote_manifest.is_none(), "fresh bucket should have no previous manifest");

    uploader
        .upload(&output_dir, &files, 1_700_000_000, &local_manifest, remote_manifest.as_ref())
        .await?;

    let body =
        get_object_bytes(s3, bucket, "first-run/static_files/headers-0-499999.tar.zst").await?;
    assert_eq!(body.as_slice(), b"fresh-chunk", "static file should be uploaded on first run");

    Ok(())
}

/// E2E test: creates a real datadir with mdbx + static files, skips compression
/// for a finalized chunk range, and verifies only the tip chunk is compressed.
#[tokio::test]
#[serial]
async fn selective_compression_skips_finalized_chunks() -> Result<()> {
    // Create a real datadir with mdbx + 4 header chunk ranges
    // block=2M, bpf=500k → 4 chunks, tip=chunk3, buffer=2 → skip chunk 0
    let source = tempfile::tempdir()?;
    let db_dir = source.path().join("db");
    std::fs::create_dir_all(&db_dir)?;
    std::fs::write(db_dir.join("mdbx.dat"), b"test-state-data")?;

    let sf_dir = source.path().join("static_files");
    std::fs::create_dir_all(&sf_dir)?;
    for component in [
        "headers",
        "transactions",
        "transaction-senders",
        "receipts",
        "account-change-sets",
        "storage-change-sets",
    ] {
        for i in 0..4u64 {
            let start = i * 500_000;
            let end = (i + 1) * 500_000 - 1;
            std::fs::write(sf_dir.join(format!("static_file_{component}_{start}_{end}")), b"data")?;
        }
    }

    // Simulate all chunked components existing remotely for range 0-499999
    let chunk_components = [
        "headers",
        "transactions",
        "transaction_senders",
        "receipts",
        "account_changesets",
        "storage_changesets",
    ];
    let mut remote: HashMap<String, u64> = HashMap::new();
    for component in chunk_components {
        remote.insert(format!("{component}-0-499999.tar.zst"), 0);
    }

    let output = tempfile::tempdir()?;
    let files = SnapshotGenerator::generate_manifest(
        source.path(),
        output.path(),
        8453,
        Some(2_000_000),
        Some(500_000),
        &remote,
    )?;

    let filenames: Vec<String> = files
        .iter()
        .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();

    // Skipped range: chunk 0 should NOT be compressed (all components exist remotely)
    for component in chunk_components {
        assert!(
            !filenames.contains(&format!("{component}-0-499999.tar.zst")),
            "{component} finalized range should not produce an archive"
        );
    }

    // Buffer + tip ranges: should be compressed
    for component in chunk_components {
        assert!(
            filenames.contains(&format!("{component}-500000-999999.tar.zst")),
            "{component} tip range should produce an archive"
        );
    }

    // Always-upload: state + manifest
    assert!(filenames.contains(&"state.tar.zst".to_string()), "state should always be produced");
    assert!(filenames.contains(&"manifest.json".to_string()), "manifest should always be produced");

    // Verify tip archive is a valid compressed file (not empty)
    let tip_path = output.path().join("headers-500000-999999.tar.zst");
    assert!(std::fs::metadata(&tip_path)?.len() > 0, "tip archive should not be empty");

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
        None,
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

    let upload_prefix = uploader
        .upload(&output_dir, &files, 1_700_000_000, &empty_manifest(2_000_000), None)
        .await?;
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
        None,
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
        public_base_url: None,
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
    let local_manifest = parse_local_manifest(&output_dir)?;

    let uploader = SnapshotUploader::new(
        harness.storage_client.clone(),
        harness.bucket_name.clone(),
        "e2e-test".to_string(),
        None,
    );
    let upload_prefix =
        uploader.upload(&output_dir, &files, 1_700_000_000, &local_manifest, None).await?;

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
