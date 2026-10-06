//! End-to-end proofs download: manifest, concurrent download and extraction,
//! verification, and cache cleanup.

use std::{path::Path, pin::pin, sync::Arc, time::Instant};

use base_reth_cli::{OutputFileChecksum, ProgressDisplay};
use eyre::Result;
use rayon::prelude::*;
use reth_cli_commands::download::DownloadDefaults;
use tracing::{info, warn};

use super::{
    ProofsArchiveAvailability, ProofsArchiveDownload, ProofsArchiveExtractor, ProofsManifest,
    ProofsManifestEntry,
};

/// Downloads and extracts the proofs database from a snapshot manifest.
///
/// Only the proofs component goes through this path. Every other snapshot
/// component is downloaded and extracted by reth's downloader.
#[derive(Debug)]
pub struct ProofsDownloader;

impl ProofsDownloader {
    /// Runs the full proofs download for `chain_id`, using `manifest_url` when
    /// given and otherwise the latest manifest from the snapshot API.
    pub async fn run(
        target_dir: &Path,
        chain_id: u64,
        concurrency: usize,
        manifest_url: Option<String>,
    ) -> Result<()> {
        let manifest_url = match manifest_url {
            Some(url) => url,
            None => {
                let api_url = DownloadDefaults::get_global().snapshot_api_url.as_ref();
                ProofsManifest::discover_latest_url(api_url, chain_id).await?
            }
        };

        Self::run_from_manifest(target_dir, &manifest_url, concurrency).await
    }

    /// Runs the full proofs download from a manifest URL.
    pub async fn run_from_manifest(
        target_dir: &Path,
        manifest_url: &str,
        concurrency: usize,
    ) -> Result<()> {
        let entry = ProofsManifest::fetch_entry(manifest_url).await?;
        Self::download_and_extract(
            &entry,
            target_dir,
            concurrency,
            ProofsArchiveDownload::DEFAULT_PIECE_SIZE,
        )
        .await
    }

    /// Downloads the archive into `target_dir/.snapshot-cache` while
    /// extracting its available prefix into `target_dir`, then verifies the
    /// extracted files and removes the cache.
    ///
    /// A download error stops extraction, and an extraction error cancels the
    /// download. On failure the cache is kept: an incomplete download resumes
    /// on the next run, and a complete archive is renamed to its final name
    /// for inspection and downloaded again next time.
    pub async fn download_and_extract(
        entry: &ProofsManifestEntry,
        target_dir: &Path,
        concurrency: usize,
        piece_size: u64,
    ) -> Result<()> {
        let cache_dir = target_dir.join(".snapshot-cache");
        tokio::fs::create_dir_all(&cache_dir).await?;
        let archive_path = cache_dir.join(&entry.file_name);
        tokio::fs::remove_file(&archive_path).await.ok();

        let download = ProofsArchiveDownload::new(entry, &cache_dir, concurrency, piece_size);
        let pieces = download.prepare().await?;
        let already_downloaded = pieces.is_complete();
        let availability = Arc::new(ProofsArchiveAvailability::new(entry.expected_size));
        let abort_guard = availability.abort_on_drop();

        info!(target: "reth::cli", "Downloading and extracting proofs archive");
        let mut extraction = {
            let part_path = download.part_path.clone();
            let target_dir = target_dir.to_path_buf();
            let availability = Arc::clone(&availability);
            let output_files = entry.output_files.clone();
            tokio::task::spawn_blocking(move || {
                ProofsArchiveExtractor::extract(&part_path, availability, &target_dir)?;
                Self::verify_output_files(&target_dir, &output_files)
            })
        };
        let mut downloading = pin!(download.run(pieces, &availability));

        let result = tokio::select! {
            downloaded = &mut downloading => match downloaded {
                Ok(()) => Self::extraction_result(extraction.await),
                Err(error) => {
                    availability.abort();
                    extraction.await.ok();
                    Err(error)
                }
            },
            extracted = &mut extraction => match Self::extraction_result(extracted) {
                // Extraction reads to the end of the archive, so the download
                // has published every byte and only needs to finish cleanup.
                Ok(()) => (&mut downloading).await,
                Err(error) => Err(error),
            },
        };
        drop(abort_guard);

        if let Err(error) = result {
            // Extraction can fail after the last piece is published but before
            // the download removes its sidecar, or before the download of an
            // already complete archive is first polled, so completeness is
            // not read back from the files on disk.
            if already_downloaded || availability.is_complete() {
                tokio::fs::remove_file(&download.sidecar_path).await.ok();
                tokio::fs::rename(&download.part_path, &archive_path).await.ok();
            }
            return Err(error);
        }

        tokio::fs::remove_dir_all(&cache_dir).await.ok();
        info!(target: "reth::cli", "Proofs database download complete");
        Ok(())
    }

    /// Flattens the extraction task result, surfacing panics as errors.
    fn extraction_result(joined: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
        joined.map_err(|error| eyre::eyre!("proofs extraction task failed: {error}"))?
    }

    /// Verifies that every manifest output file exists under `target_dir` with the expected size
    /// and BLAKE3 hash.
    ///
    /// Files are hashed in parallel on the Rayon pool. Manifests without `output_files` are
    /// accepted with a warning, since they carry nothing to verify against.
    fn verify_output_files(target_dir: &Path, output_files: &[OutputFileChecksum]) -> Result<()> {
        if output_files.is_empty() {
            warn!(target: "reth::cli", "Manifest lists no proofs output files, skipping verification");
            return Ok(());
        }

        let total_bytes: u64 = output_files.iter().map(|file| file.size).sum();
        info!(
            target: "reth::cli",
            files = output_files.len(),
            size = %ProgressDisplay::bytes(total_bytes as f64),
            "Verifying extracted proofs files"
        );
        let started = Instant::now();

        output_files.par_iter().try_for_each(|expected| {
            let path = target_dir.join(&expected.path);
            let file = std::fs::File::open(&path)
                .map_err(|e| eyre::eyre!("missing proofs file {}: {e}", expected.path))?;
            let size = file.metadata()?.len();
            if size != expected.size {
                eyre::bail!(
                    "proofs file {} has size {size}, manifest expects {}",
                    expected.path,
                    expected.size
                );
            }
            let hash = blake3::Hasher::new().update_reader(file)?.finalize().to_hex();
            if !hash.as_str().eq_ignore_ascii_case(&expected.blake3) {
                eyre::bail!(
                    "proofs file {} has BLAKE3 {hash}, manifest expects {}",
                    expected.path,
                    expected.blake3
                );
            }
            Ok(())
        })?;

        info!(
            target: "reth::cli",
            files = output_files.len(),
            elapsed = ?started.elapsed(),
            "Verified extracted proofs files"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
    };
    use tokio::sync::Mutex;

    use super::*;
    use crate::commands::download::proofs::test_utils::{
        create_proofs_archive, generate_proofs_snapshot, parse_byte_range, partial_content,
        proofs_entry, serve, start_test_server,
    };

    /// Piece size that splits the streaming test archives into many pieces.
    const TEST_PIECE_SIZE: u64 = 16 * 1024;

    /// Bound on each streaming test so a hang fails instead of blocking the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Returns `len` bytes that zstd cannot compress, so compressed offsets
    /// track tar offsets.
    fn incompressible(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn full_pipeline_verifies_generated_snapshot() {
        let (manifest, archive) = generate_proofs_snapshot(&[
            ("CURRENT", b"MANIFEST-000014\n"),
            ("000060.sst", b"sst-data"),
            ("nested/000801.log", b"wal-data"),
        ]);
        assert_eq!(manifest["components"]["proofs"]["output_files"].as_array().unwrap().len(), 3);

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        ProofsDownloader::run_from_manifest(target.path(), &manifest_url, 1)
            .await
            .expect("generated snapshot should download and verify");

        assert_eq!(std::fs::read(target.path().join("proofs/000060.sst")).unwrap(), b"sst-data");
        assert_eq!(
            std::fs::read(target.path().join("proofs/nested/000801.log")).unwrap(),
            b"wal-data"
        );
        assert!(!target.path().join(".snapshot-cache").exists(), "cache should be cleaned up");

        handle.abort();
    }

    #[tokio::test]
    async fn full_pipeline_rejects_hash_mismatch() {
        let (mut manifest, archive) =
            generate_proofs_snapshot(&[("000060.sst", b"sst-data"), ("CURRENT", b"current")]);
        let output_files = manifest["components"]["proofs"]["output_files"].as_array_mut().unwrap();
        let sst = output_files.iter_mut().find(|file| file["path"] == "proofs/000060.sst").unwrap();
        sst["blake3"] = serde_json::Value::String("00".repeat(32));

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        let error = ProofsDownloader::run_from_manifest(target.path(), &manifest_url, 1)
            .await
            .expect_err("hash mismatch should fail the download");

        assert!(error.to_string().contains("proofs/000060.sst"), "error: {error}");
        assert!(
            target.path().join(".snapshot-cache/proofs.tar.zst").exists(),
            "archive should be kept for inspection"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn full_pipeline_rejects_size_mismatch() {
        let (mut manifest, archive) = generate_proofs_snapshot(&[("000060.sst", b"sst-data")]);
        manifest["components"]["proofs"]["output_files"][0]["size"] = serde_json::json!(9);

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        let error = ProofsDownloader::run_from_manifest(target.path(), &manifest_url, 1)
            .await
            .expect_err("size mismatch should fail the download");

        assert!(error.to_string().contains("size 8"), "error: {error}");

        handle.abort();
    }

    #[tokio::test]
    async fn full_pipeline_rejects_archive_missing_manifest_file() {
        // The manifest lists a file that the archive does not contain, as happens when an
        // archive is cut short at a tar entry boundary.
        let (mut manifest, archive) =
            generate_proofs_snapshot(&[("000060.sst", b"sst-data"), ("CURRENT", b"current")]);
        let output_files = manifest["components"]["proofs"]["output_files"].as_array_mut().unwrap();
        let mut missing = output_files[0].clone();
        missing["path"] = serde_json::Value::String("proofs/000061.sst".to_string());
        output_files.push(missing);

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        let error = ProofsDownloader::run_from_manifest(target.path(), &manifest_url, 1)
            .await
            .expect_err("missing file should fail the download");

        assert!(
            error.to_string().contains("missing proofs file proofs/000061.sst"),
            "error: {error}"
        );

        handle.abort();
    }

    #[derive(Clone)]
    struct GatedLastPieceState {
        data: Vec<u8>,
        extracted_file: PathBuf,
        extracted_len: u64,
        saw_extracted_file: Arc<AtomicBool>,
    }

    /// Holds the final piece until `extracted_file` is fully written, proving
    /// extraction runs before the download finishes.
    async fn handle_gated_last_piece(
        State(state): State<GatedLastPieceState>,
        headers: HeaderMap,
    ) -> Response {
        let Some((start, end)) = parse_byte_range(&headers, state.data.len()) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if end + 1 == state.data.len() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let len = std::fs::metadata(&state.extracted_file).map(|m| m.len()).unwrap_or(0);
                if len == state.extracted_len {
                    state.saw_extracted_file.store(true, Ordering::SeqCst);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        partial_content(&state.data, start, end)
    }

    #[tokio::test]
    async fn extraction_runs_while_download_is_in_progress() {
        let first = incompressible(256 * 1024, 1);
        let second = incompressible(256 * 1024, 2);
        let archive =
            create_proofs_archive(&[("proofs/first.bin", &first), ("proofs/second.bin", &second)]);
        let target = tempfile::tempdir().unwrap();
        let saw_extracted_file = Arc::new(AtomicBool::new(false));
        let (base_url, handle) =
            serve(Router::new().route("/proofs.tar.zst", get(handle_gated_last_piece)).with_state(
                GatedLastPieceState {
                    data: archive.clone(),
                    extracted_file: target.path().join("proofs/first.bin"),
                    extracted_len: first.len() as u64,
                    saw_extracted_file: Arc::clone(&saw_extracted_file),
                },
            ))
            .await;

        tokio::time::timeout(
            TEST_TIMEOUT,
            ProofsDownloader::download_and_extract(
                &proofs_entry(&base_url, &archive),
                target.path(),
                2,
                TEST_PIECE_SIZE,
            ),
        )
        .await
        .expect("pipeline should not hang")
        .expect("pipeline should succeed");

        assert!(
            saw_extracted_file.load(Ordering::SeqCst),
            "the first file should be extracted before the last piece is served"
        );
        assert_eq!(std::fs::read(target.path().join("proofs/first.bin")).unwrap(), first);
        assert_eq!(std::fs::read(target.path().join("proofs/second.bin")).unwrap(), second);

        handle.abort();
    }

    #[derive(Clone)]
    struct SlowFirstPieceState {
        data: Vec<u8>,
        served: Arc<Mutex<Vec<usize>>>,
    }

    /// Delays the first piece so later pieces land on disk before it.
    async fn handle_slow_first_piece(
        State(state): State<SlowFirstPieceState>,
        headers: HeaderMap,
    ) -> Response {
        let Some((start, end)) = parse_byte_range(&headers, state.data.len()) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if start == 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        state.served.lock().await.push(start);
        partial_content(&state.data, start, end)
    }

    #[tokio::test]
    async fn extraction_waits_for_pieces_that_arrive_out_of_order() {
        let data = incompressible(128 * 1024, 3);
        let archive = create_proofs_archive(&[("proofs/data.bin", &data)]);
        let served = Arc::new(Mutex::new(Vec::new()));
        let (base_url, handle) =
            serve(Router::new().route("/proofs.tar.zst", get(handle_slow_first_piece)).with_state(
                SlowFirstPieceState { data: archive.clone(), served: Arc::clone(&served) },
            ))
            .await;
        let target = tempfile::tempdir().unwrap();

        tokio::time::timeout(
            TEST_TIMEOUT,
            ProofsDownloader::download_and_extract(
                &proofs_entry(&base_url, &archive),
                target.path(),
                4,
                TEST_PIECE_SIZE,
            ),
        )
        .await
        .expect("pipeline should not hang")
        .expect("pipeline should succeed");

        assert_ne!(served.lock().await.first(), Some(&0), "a later piece should be served first");
        assert_eq!(std::fs::read(target.path().join("proofs/data.bin")).unwrap(), data);

        handle.abort();
    }

    #[tokio::test]
    async fn download_failure_stops_extraction() {
        let data = incompressible(128 * 1024, 4);
        let archive = create_proofs_archive(&[("proofs/data.bin", &data)]);
        let len = archive.len();
        let served = archive.clone();
        let (base_url, handle) = serve(Router::new().route(
            "/proofs.tar.zst",
            get(move |headers: HeaderMap| {
                let data = served.clone();
                async move {
                    match parse_byte_range(&headers, len) {
                        Some((start, end)) if start < len / 2 => partial_content(&data, start, end),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        ))
        .await;
        let target = tempfile::tempdir().unwrap();

        let error = tokio::time::timeout(
            TEST_TIMEOUT,
            ProofsDownloader::download_and_extract(
                &proofs_entry(&base_url, &archive),
                target.path(),
                1,
                TEST_PIECE_SIZE,
            ),
        )
        .await
        .expect("a failed download must not leave extraction blocked")
        .expect_err("a 404 mid-download should fail the pipeline");

        assert!(error.to_string().contains("404"), "error: {error}");
        assert!(
            target.path().join(".snapshot-cache/proofs.tar.zst.part.ranges").exists(),
            "an incomplete download should keep its resume state"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn corrupt_cached_archive_is_moved_aside() {
        let archive = incompressible(4 * 1024, 6);
        let target = tempfile::tempdir().unwrap();
        let cache_dir = target.path().join(".snapshot-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("proofs.tar.zst.part"), &archive).unwrap();

        let error = ProofsDownloader::download_and_extract(
            &proofs_entry("http://127.0.0.1:9", &archive),
            target.path(),
            1,
            TEST_PIECE_SIZE,
        )
        .await
        .expect_err("a corrupt cached archive should fail extraction");

        assert!(error.to_string().contains("decode"), "error: {error}");

        assert!(
            !cache_dir.join("proofs.tar.zst.part").exists(),
            "the next run must download again instead of trusting the cached archive"
        );
        assert!(cache_dir.join("proofs.tar.zst").exists(), "archive should be kept for inspection");
    }

    #[tokio::test]
    async fn corrupt_archive_cancels_download() {
        let archive = incompressible(256 * 1024, 5);
        let served = archive.clone();
        let (base_url, handle) = serve(Router::new().route(
            "/proofs.tar.zst",
            get(move |headers: HeaderMap| {
                let data = served.clone();
                async move {
                    let Some((start, end)) = parse_byte_range(&headers, data.len()) else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    if start > 0 {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                    partial_content(&data, start, end)
                }
            }),
        ))
        .await;
        let target = tempfile::tempdir().unwrap();

        let error = tokio::time::timeout(
            TEST_TIMEOUT,
            ProofsDownloader::download_and_extract(
                &proofs_entry(&base_url, &archive),
                target.path(),
                2,
                TEST_PIECE_SIZE,
            ),
        )
        .await
        .expect("a corrupt archive should fail without waiting for the download")
        .expect_err("a non-zstd archive should fail extraction");

        assert!(error.to_string().contains("decode"), "error: {error}");

        handle.abort();
    }
}
