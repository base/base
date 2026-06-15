//! Download command wrapper that extends reth's `DownloadCommand` with `--proofs`.
//!
//! Delegates all standard snapshot components to reth's download pipeline and
//! handles the Base-specific proofs database download separately using the
//! same snapshot source and manifest.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use base_execution_chainspec::BaseChainSpec;
use clap::Parser;
use eyre::Result;
use futures::StreamExt;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::download::{DownloadCommand, DownloadDefaults};
use reth_node_core::{args::DatadirArgs, dirs::DataDirPath};
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Download Base node snapshots from R2 storage.
///
/// Wraps reth's download command with an additional `--proofs` flag that
/// downloads the expanded trie proof database for fault proof support.
///
/// When `--proofs` is passed, the command runs reth's standard download
/// then fetches and extracts the proofs archive from the same snapshot source.
#[derive(Debug, Parser)]
pub struct BaseDownloadCommand<C: ChainSpecParser> {
    #[command(flatten)]
    inner: DownloadCommand<C>,

    /// Also download the proofs database for fault proof support.
    ///
    /// After the standard download completes, fetches the proofs archive
    /// from the same snapshot source and extracts it into the data directory.
    /// Re-running with `--proofs` will overwrite any existing proofs database.
    #[arg(long)]
    proofs: bool,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> BaseDownloadCommand<C> {
    /// Executes the download command.
    pub async fn execute<N>(self) -> Result<()> {
        let Self { inner, proofs } = self;

        let (data_dir, chain_id) = if proofs {
            let chain = inner
                .chain_spec()
                .ok_or_else(|| eyre::eyre!("--proofs flag is only on Base"))?
                .chain();
            let chain_id = chain.id();
            let dir = reth_node_core::dirs::PlatformPath::<DataDirPath>::default()
                .with_chain(chain, resolve_datadir_args(std::env::args_os()));
            info!(target: "reth::cli", datadir = %dir.data_dir().display(), "Resolved datadir for proofs download");
            (Some(dir), Some(chain_id))
        } else {
            (None, None)
        };

        inner.execute::<N>().await?;

        if let (Some(data_dir), Some(chain_id)) = (data_dir, chain_id) {
            let target_dir = data_dir.data_dir().to_path_buf();
            ProofsDownloader::run(&target_dir, chain_id).await?;
        }

        Ok(())
    }
}

/// Extracts `--datadir` from the current process args without doing a second
/// permissive clap parse of the whole command.
fn resolve_datadir_args(args: impl IntoIterator<Item = OsString>) -> DatadirArgs {
    let mut datadir_args = DatadirArgs::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };

        if arg == "--datadir" {
            if let Some(value) = args.next() {
                datadir_args.datadir = PathBuf::from(value).into();
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--datadir=") {
            datadir_args.datadir = PathBuf::from(value).into();
        }
    }

    datadir_args
}

impl<C: ChainSpecParser> BaseDownloadCommand<C> {
    /// Returns the underlying chain spec.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.inner.chain_spec()
    }
}

/// Metadata parsed from the manifest's `proofs` component.
#[derive(Debug)]
struct ProofsManifestEntry {
    file_name: String,
    expected_size: u64,
    archive_url: String,
}

/// Downloads the proofs database from a snapshot manifest.
///
/// Encapsulates the full pipeline: manifest fetch → archive download with
/// resume → size verification → tar+zstd extraction → cache cleanup.
#[derive(Debug)]
struct ProofsDownloader;

impl ProofsDownloader {
    /// Runs the full proofs download pipeline for the given chain.
    async fn run(target_dir: &Path, chain_id: u64) -> Result<()> {
        let defaults = DownloadDefaults::get_global();
        let base_url =
            defaults.default_chain_aware_base_url.as_deref().unwrap_or(&defaults.default_base_url);
        let manifest_url = format!("{base_url}/{chain_id}/manifest.json");

        Self::run_from_manifest(target_dir, &manifest_url).await
    }

    /// Runs the full proofs download pipeline from a manifest URL.
    async fn run_from_manifest(target_dir: &Path, manifest_url: &str) -> Result<()> {
        let entry = Self::fetch_manifest_entry(manifest_url).await?;

        let cache_dir = target_dir.join(".snapshot-cache");
        tokio::fs::create_dir_all(&cache_dir).await?;

        let archive_path = Self::download_archive(&entry, &cache_dir).await?;

        Self::extract_and_cleanup(&archive_path, target_dir, &cache_dir).await
    }

    /// Fetches the manifest and extracts the proofs component metadata.
    async fn fetch_manifest_entry(manifest_url: &str) -> Result<ProofsManifestEntry> {
        info!(target: "reth::cli", manifest_url = %manifest_url, "Fetching manifest for proofs component");

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        let manifest: serde_json::Value = client
            .get(manifest_url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .error_for_status()
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .json()
            .await
            .map_err(|e| eyre::eyre!("failed to parse manifest from {manifest_url}: {e}"))?;

        let proofs_component =
            manifest.get("components").and_then(|c| c.get("proofs")).ok_or_else(|| {
                eyre::eyre!(
                    "manifest has no 'proofs' component — this snapshot does not include proofs"
                )
            })?;

        let file_name = proofs_component
            .get("file")
            .and_then(|f| f.as_str())
            .ok_or_else(|| eyre::eyre!("proofs component missing 'file' field in manifest"))?
            .to_string();

        let expected_size = proofs_component
            .get("size")
            .and_then(|s| s.as_u64())
            .ok_or_else(|| eyre::eyre!("proofs component missing 'size' field in manifest"))?;

        let file_path = std::path::Path::new(&file_name);
        if file_path.is_absolute()
            || file_name.contains("..")
            || file_path.components().count() != 1
        {
            eyre::bail!("invalid proofs file name in manifest: {file_name}");
        }

        let archive_base_url = manifest_url
            .rsplit_once('/')
            .map(|(base, _)| base.to_string())
            .ok_or_else(|| eyre::eyre!("malformed manifest URL: {manifest_url}"))?;

        let archive_url = format!("{archive_base_url}/{file_name}");

        Ok(ProofsManifestEntry { file_name, expected_size, archive_url })
    }

    /// Downloads the proofs archive with resume support and size verification.
    async fn download_archive(
        entry: &ProofsManifestEntry,
        cache_dir: &Path,
    ) -> Result<std::path::PathBuf> {
        let dest_path = cache_dir.join(&entry.file_name);
        let part_path = cache_dir.join(format!("{}.part", entry.file_name));

        let mut existing_size = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);

        if existing_size == entry.expected_size {
            info!(target: "reth::cli", "Part file already matches expected size, skipping download");
            tokio::fs::rename(&part_path, &dest_path).await?;
            return Ok(dest_path);
        }

        if existing_size > entry.expected_size {
            info!(
                target: "reth::cli",
                existing_size,
                expected_size = entry.expected_size,
                "Part file exceeds expected size, restarting proofs download"
            );
            tokio::fs::remove_file(&part_path).await.ok();
            existing_size = 0;
        }

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;

        let mut request = client.get(&entry.archive_url);
        if existing_size > 0 {
            request = request.header("Range", format!("bytes={existing_size}-"));
            info!(target: "reth::cli", resume_from = existing_size, "Resuming proofs download");
        }

        info!(target: "reth::cli", url = %entry.archive_url, "Downloading proofs database");

        let response = request.send().await.map_err(|e| {
            eyre::eyre!("failed to download proofs from {}: {e}", entry.archive_url)
        })?;
        let status = response.status();

        if !status.is_success() {
            eyre::bail!("proofs download failed with HTTP {status}: {}", entry.archive_url);
        }

        let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

        if existing_size > 0 && !is_resume {
            info!(target: "reth::cli", "Server returned full response despite range request, restarting download");
            tokio::fs::remove_file(&part_path).await.ok();
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(is_resume)
            .write(!is_resume)
            .truncate(!is_resume)
            .open(&part_path)
            .await?;

        let mut downloaded: u64 = if is_resume { existing_size } else { 0 };
        let mut last_log = tokio::time::Instant::now();

        let mut stream = response.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| {
                eyre::eyre!("stream interrupted downloading {}: {e}", entry.archive_url)
            })?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;
            if last_log.elapsed() >= std::time::Duration::from_secs(30) {
                info!(
                    target: "reth::cli",
                    downloaded_mb = downloaded / (1024 * 1024),
                    expected_mb = entry.expected_size / (1024 * 1024),
                    "Proofs download progress"
                );
                last_log = tokio::time::Instant::now();
            }
        }
        file.shutdown().await?;

        let downloaded_size = tokio::fs::metadata(&part_path).await?.len();
        if downloaded_size != entry.expected_size {
            tokio::fs::remove_file(&part_path).await.ok();
            eyre::bail!(
                "proofs archive size mismatch: downloaded {downloaded_size} bytes, \
                 manifest declares {} bytes — archive may be truncated or corrupt",
                entry.expected_size
            );
        }

        tokio::fs::rename(&part_path, &dest_path).await?;
        Ok(dest_path)
    }

    /// Extracts the archive and cleans up the cache directory.
    async fn extract_and_cleanup(
        archive_path: &Path,
        target_dir: &Path,
        cache_dir: &Path,
    ) -> Result<()> {
        info!(target: "reth::cli", "Extracting proofs archive");

        let extract_target = target_dir.to_path_buf();
        let extract_path = archive_path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::extract_tar_zst(&extract_path, &extract_target))
            .await??;

        tokio::fs::remove_file(archive_path).await.ok();
        tokio::fs::remove_dir_all(cache_dir).await.ok();

        info!(target: "reth::cli", "Proofs database download complete");
        Ok(())
    }

    /// Extracts a `.tar.zst` archive into the target directory.
    fn extract_tar_zst(archive_path: &Path, target_dir: &Path) -> Result<()> {
        let file = std::fs::File::open(archive_path)
            .map_err(|e| eyre::eyre!("failed to open {}: {e}", archive_path.display()))?;
        let decoder = zstd::Decoder::new(file)?;
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(target_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use clap::Parser;

    use super::*;
    use crate::chainspec::BaseChainSpecParser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: BaseDownloadCommand<BaseChainSpecParser>,
    }

    fn create_proofs_archive(content_pairs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        let encoder = zstd::Encoder::new(&mut buf, 0).unwrap();
        let mut builder = tar::Builder::new(encoder);

        for (path, data) in content_pairs {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        buf
    }

    async fn start_test_server(
        manifest_json: serde_json::Value,
        archive_bytes: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();
        let manifest_clone = manifest_bytes.clone();
        let archive_clone = archive_bytes.clone();

        let app = Router::new()
            .route(
                "/manifest.json",
                get(move || {
                    let data = manifest_clone.clone();
                    async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], data) }
                }),
            )
            .route(
                "/proofs.tar.zst",
                get(move || {
                    let data = archive_clone.clone();
                    async move { data }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manifest_url = format!("http://127.0.0.1:{}/manifest.json", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (manifest_url, handle)
    }

    async fn start_range_aware_server(
        archive_bytes: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app =
            Router::new().route("/proofs.tar.zst", get(handle_range)).with_state(archive_bytes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (base_url, handle)
    }

    async fn handle_range(State(data): State<Vec<u8>>, headers: HeaderMap) -> impl IntoResponse {
        if let Some(range) = headers.get("Range").and_then(|v| v.to_str().ok())
            && let Some(start_str) = range.strip_prefix("bytes=")
            && let Ok(start) = start_str.trim_end_matches('-').parse::<usize>()
            && start < data.len()
        {
            return (
                StatusCode::PARTIAL_CONTENT,
                [(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, data.len() - 1, data.len()),
                )],
                data[start..].to_vec(),
            )
                .into_response();
        }
        (StatusCode::OK, data).into_response()
    }

    #[test]
    fn proofs_flag_is_parsed() {
        let cli = TestCli::parse_from(["test", "--proofs"]);
        assert!(cli.args.proofs, "--proofs should be true");
    }

    #[test]
    fn download_without_proofs_flag() {
        let cli = TestCli::parse_from(["test"]);
        assert!(!cli.args.proofs, "--proofs should default to false");
    }

    #[test]
    fn resolve_datadir_args_reads_explicit_flag() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir"),
            OsString::from("/tmp/base-download-test"),
        ]);

        assert_eq!(
            datadir.datadir,
            PathBuf::from("/tmp/base-download-test").into(),
            "resolver should read --datadir VALUE"
        );
    }

    #[test]
    fn resolve_datadir_args_reads_equals_syntax() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir=/tmp/base-download-test"),
        ]);

        assert_eq!(
            datadir.datadir,
            PathBuf::from("/tmp/base-download-test").into(),
            "resolver should read --datadir=VALUE"
        );
    }

    #[tokio::test]
    async fn fetch_manifest_entry_extracts_proofs_metadata() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);
        let manifest = serde_json::json!({
            "block": 1000000,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "proofs.tar.zst",
                    "size": archive.len(),
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, archive.clone()).await;
        let entry = ProofsDownloader::fetch_manifest_entry(&manifest_url).await.unwrap();

        assert_eq!(entry.file_name, "proofs.tar.zst");
        assert_eq!(entry.expected_size, archive.len() as u64);
        assert!(entry.archive_url.ends_with("/proofs.tar.zst"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_rejects_path_traversal() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "../../etc/evil.tar.zst",
                    "size": 100,
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err(), "path traversal should be rejected");
        assert!(result.unwrap_err().to_string().contains("invalid proofs file name"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_fails_when_no_proofs() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "state": { "file": "state.tar.zst", "size": 100, "decompressed_size": 500, "output_files": [] }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no 'proofs' component"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_manifest_entry_fails_when_size_missing() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": { "file": "proofs.tar.zst", "decompressed_size": 0, "output_files": [] }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
        let result = ProofsDownloader::fetch_manifest_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'size'"));

        handle.abort();
    }

    #[tokio::test]
    async fn full_pipeline_downloads_and_extracts() {
        let archive = create_proofs_archive(&[
            ("proofs/data.mdb", b"real-proof-data-from-server"),
            ("proofs/lock.mdb", b"lock-file"),
        ]);

        let manifest = serde_json::json!({
            "block": 1000000,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "proofs.tar.zst",
                    "size": archive.len(),
                    "decompressed_size": 0,
                    "output_files": []
                }
            }
        });

        let (manifest_url, handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        ProofsDownloader::run_from_manifest(target.path(), &manifest_url)
            .await
            .expect("full pipeline should succeed");

        assert_eq!(
            std::fs::read(target.path().join("proofs/data.mdb")).unwrap(),
            b"real-proof-data-from-server",
            "extracted content should match"
        );
        assert_eq!(std::fs::read(target.path().join("proofs/lock.mdb")).unwrap(), b"lock-file");
        assert!(!target.path().join(".snapshot-cache").exists(), "cache should be cleaned up");

        handle.abort();
    }

    #[test]
    fn extract_tar_zst_creates_files() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let archive_path = src.path().join("proofs.tar.zst");
        std::fs::write(
            &archive_path,
            create_proofs_archive(&[("proofs/data.mdb", b"proof-data-contents")]),
        )
        .unwrap();

        ProofsDownloader::extract_tar_zst(&archive_path, dest.path()).unwrap();

        let extracted = dest.path().join("proofs/data.mdb");
        assert!(extracted.exists(), "extracted file should exist");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"proof-data-contents");
    }

    #[test]
    fn extract_tar_zst_preserves_directory_structure() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();

        let archive_path = src.path().join("proofs.tar.zst");
        std::fs::write(
            &archive_path,
            create_proofs_archive(&[
                ("proofs/data.mdb", b"data"),
                ("proofs/lock.mdb", b"lock"),
                ("proofs/nested/deep.dat", b"deep"),
            ]),
        )
        .unwrap();

        ProofsDownloader::extract_tar_zst(&archive_path, dest.path()).unwrap();

        assert!(dest.path().join("proofs/data.mdb").exists());
        assert!(dest.path().join("proofs/lock.mdb").exists());
        assert!(dest.path().join("proofs/nested/deep.dat").exists());
        assert_eq!(std::fs::read(dest.path().join("proofs/nested/deep.dat")).unwrap(), b"deep");
    }

    #[test]
    fn extract_tar_zst_fails_on_missing_archive() {
        let dest = tempfile::tempdir().unwrap();
        let result = ProofsDownloader::extract_tar_zst(
            &dest.path().join("nonexistent.tar.zst"),
            dest.path(),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn download_archive_resumes_from_partial_file() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"complete-proof-data")]);
        let (base_url, handle) = start_range_aware_server(archive.clone()).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");

        let half = archive.len() / 2;
        std::fs::write(&part_path, &archive[..half]).unwrap();
        assert_eq!(
            std::fs::metadata(&part_path).unwrap().len(),
            half as u64,
            "part file should contain first half of archive"
        );

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path()).await.unwrap();
        let downloaded = std::fs::read(&dest).unwrap();

        assert_eq!(downloaded.len(), archive.len(), "resumed download should produce full archive");
        assert_eq!(downloaded, archive, "resumed archive should match original byte-for-byte");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_restarts_when_server_ignores_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"fresh-data")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let data = archive.clone();
            let app = Router::new().route(
                "/proofs.tar.zst",
                get(move || {
                    let d = data.clone();
                    async move { d }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, b"stale-garbage-data-from-old-snapshot").unwrap();

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path()).await.unwrap();
        let downloaded = std::fs::read(&dest).unwrap();

        assert_eq!(downloaded, archive, "should discard stale .part and download fresh archive");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_uses_completed_part_file_without_requesting_range() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"already-complete")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let app = Router::new().route(
                "/proofs.tar.zst",
                get(|| async { (StatusCode::RANGE_NOT_SATISFIABLE, Vec::<u8>::new()) }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let part_path = cache_dir.path().join("proofs.tar.zst.part");
        std::fs::write(&part_path, &archive).unwrap();

        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let dest = ProofsDownloader::download_archive(&entry, cache_dir.path()).await.unwrap();

        assert_eq!(dest, cache_dir.path().join("proofs.tar.zst"));
        assert_eq!(std::fs::read(&dest).unwrap(), archive);
        assert!(!Path::new(&part_path).exists(), "completed .part should be renamed into place");

        handle.abort();
    }

    #[tokio::test]
    async fn download_archive_fails_on_size_mismatch() {
        let archive = create_proofs_archive(&[("proofs/data.mdb", b"data")]);

        let (base_url, handle) = {
            use axum::{Router, routing::get};

            let data = archive.clone();
            let app = Router::new().route(
                "/proofs.tar.zst",
                get(move || {
                    let d = data.clone();
                    async move { d }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://127.0.0.1:{}", addr.port());
            let h = tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
            (url, h)
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let entry = ProofsManifestEntry {
            file_name: "proofs.tar.zst".to_string(),
            expected_size: archive.len() as u64 + 999,
            archive_url: format!("{base_url}/proofs.tar.zst"),
        };

        let result = ProofsDownloader::download_archive(&entry, cache_dir.path()).await;

        assert!(result.is_err(), "size mismatch should fail");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "error should mention size mismatch, got: {err}");

        assert!(
            !cache_dir.path().join("proofs.tar.zst.part").exists(),
            "corrupt .part file should be deleted on size mismatch"
        );

        handle.abort();
    }
}
