//! Download command wrapper that extends reth's `DownloadCommand` with `--proofs`.
//!
//! Delegates all standard snapshot components to reth's download pipeline and
//! handles the Base-specific proofs database download separately using the
//! same snapshot source and manifest.

use std::sync::Arc;

use base_execution_chainspec::BaseChainSpec;
use clap::Parser;
use eyre::Result;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::download::DownloadDefaults;
use reth_node_core::dirs::DataDirPath;
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
    inner: reth_cli_commands::download::DownloadCommand<C>,

    /// Also download the proofs database for fault proof support.
    ///
    /// After the standard download completes, fetches the proofs archive
    /// from the same snapshot source and extracts it into the data directory.
    #[arg(long)]
    proofs: bool,
}

impl<C: ChainSpecParser<ChainSpec = BaseChainSpec>> BaseDownloadCommand<C> {
    /// Executes the download command.
    pub async fn execute<N>(self) -> Result<()> {
        let proofs = self.proofs;

        let (data_dir, chain_id) = if proofs {
            let chain = self.inner.chain_spec().map(|cs| cs.chain()).unwrap_or_default();
            let dir = reth_node_core::dirs::PlatformPath::<DataDirPath>::default()
                .with_chain(chain, reth_node_core::args::DatadirArgs::default());
            let id = chain.id();
            (Some(dir), Some(id))
        } else {
            (None, None)
        };

        self.inner.execute::<N>().await?;

        if let (Some(data_dir), Some(chain_id)) = (data_dir, chain_id) {
            let target_dir = data_dir.data_dir().to_path_buf();
            download_proofs(&target_dir, chain_id).await?;
        }

        Ok(())
    }
}

impl<C: ChainSpecParser> BaseDownloadCommand<C> {
    /// Returns the underlying chain spec.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.inner.chain_spec()
    }
}

/// Downloads the proofs database by fetching the manifest from the configured
/// snapshot source, reading the `proofs` component, and downloading + extracting
/// the archive into `target_dir`.
async fn download_proofs(target_dir: &std::path::Path, chain_id: u64) -> Result<()> {
    let defaults = DownloadDefaults::get_global();
    let base_url =
        defaults.default_chain_aware_base_url.as_deref().unwrap_or(&defaults.default_base_url);
    let manifest_url = format!("{base_url}/{chain_id}/manifest.json");

    download_proofs_from_manifest(target_dir, &manifest_url).await
}

/// Fetches a manifest, locates the proofs component, downloads the archive,
/// and extracts it into `target_dir`.
async fn download_proofs_from_manifest(
    target_dir: &std::path::Path,
    manifest_url: &str,
) -> Result<()> {
    info!(target: "reth::cli", manifest_url = %manifest_url, "Fetching manifest for proofs component");

    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(600)).build()?;

    let manifest: serde_json::Value = client
        .get(manifest_url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
        .json()
        .await?;

    let proofs_component =
        manifest.get("components").and_then(|c| c.get("proofs")).ok_or_else(|| {
            eyre::eyre!(
                "manifest has no 'proofs' component — this snapshot does not include proofs"
            )
        })?;

    let proofs_file = proofs_component
        .get("file")
        .and_then(|f| f.as_str())
        .ok_or_else(|| eyre::eyre!("proofs component missing 'file' field in manifest"))?;

    let archive_base_url =
        manifest.get("base_url").and_then(|u| u.as_str()).map(|u| u.to_string()).unwrap_or_else(
            || manifest_url.rsplit_once('/').map(|(base, _)| base.to_string()).unwrap_or_default(),
        );

    let proofs_url = format!("{archive_base_url}/{proofs_file}");

    info!(target: "reth::cli", url = %proofs_url, target = %target_dir.display(), "Downloading proofs database");

    let cache_dir = target_dir.join(".snapshot-cache");
    tokio::fs::create_dir_all(&cache_dir).await?;

    let dest_path = cache_dir.join(proofs_file);
    let part_path = cache_dir.join(format!("{proofs_file}.part"));

    let existing_size = tokio::fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0);

    let mut request = client.get(&proofs_url);
    if existing_size > 0 {
        request = request.header("Range", format!("bytes={existing_size}-"));
        info!(target: "reth::cli", resume_from = existing_size, "Resuming proofs download");
    }

    let response = request.send().await?;
    let status = response.status();

    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        eyre::bail!("proofs download failed with HTTP {status}: {proofs_url}");
    }

    let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(is_resume)
        .write(!is_resume)
        .truncate(!is_resume)
        .open(&part_path)
        .await?;

    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut stream = response.bytes_stream();
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;

    tokio::fs::rename(&part_path, &dest_path).await?;

    info!(target: "reth::cli", "Extracting proofs archive");

    let extract_target = target_dir.to_path_buf();
    let extract_path = dest_path.clone();
    tokio::task::spawn_blocking(move || extract_tar_zst(&extract_path, &extract_target)).await??;

    tokio::fs::remove_file(&dest_path).await.ok();
    tokio::fs::remove_dir_all(&cache_dir).await.ok();

    info!(target: "reth::cli", "Proofs database download complete");
    Ok(())
}

/// Extracts a `.tar.zst` archive into the target directory.
fn extract_tar_zst(archive_path: &std::path::Path, target_dir: &std::path::Path) -> Result<()> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| eyre::eyre!("failed to open {}: {e}", archive_path.display()))?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(target_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
        use axum::{Router, routing::get};

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

    #[tokio::test]
    async fn download_proofs_fetches_archive_from_manifest() {
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

        let (manifest_url, server_handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        download_proofs_from_manifest(target.path(), &manifest_url)
            .await
            .expect("download_proofs should succeed");

        let data_path = target.path().join("proofs/data.mdb");
        assert!(data_path.exists(), "proofs/data.mdb should be extracted");
        assert_eq!(
            std::fs::read(&data_path).unwrap(),
            b"real-proof-data-from-server",
            "extracted content should match what the server served"
        );

        let lock_path = target.path().join("proofs/lock.mdb");
        assert!(lock_path.exists(), "proofs/lock.mdb should be extracted");
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"lock-file");

        server_handle.abort();
    }

    #[tokio::test]
    async fn download_proofs_cleans_up_cache_on_success() {
        let archive = create_proofs_archive(&[("proofs/tiny.dat", b"x")]);
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "proofs": {
                    "file": "proofs.tar.zst",
                    "size": archive.len(),
                    "decompressed_size": 1,
                    "output_files": []
                }
            }
        });

        let (manifest_url, server_handle) = start_test_server(manifest, archive).await;
        let target = tempfile::tempdir().unwrap();

        download_proofs_from_manifest(target.path(), &manifest_url).await.unwrap();

        assert!(
            !target.path().join(".snapshot-cache").exists(),
            ".snapshot-cache should be cleaned up after extraction"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn download_proofs_fails_when_manifest_has_no_proofs() {
        let manifest = serde_json::json!({
            "block": 100,
            "chain_id": 8453,
            "storage_version": 2,
            "timestamp": 1700000000,
            "components": {
                "state": {
                    "file": "state.tar.zst",
                    "size": 100,
                    "decompressed_size": 500,
                    "output_files": []
                }
            }
        });

        let (manifest_url, server_handle) = start_test_server(manifest, vec![]).await;
        let target = tempfile::tempdir().unwrap();

        let result = download_proofs_from_manifest(target.path(), &manifest_url).await;

        assert!(result.is_err(), "should fail when manifest has no proofs component");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no 'proofs' component"),
            "error should explain proofs is missing, got: {err}"
        );

        server_handle.abort();
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

        extract_tar_zst(&archive_path, dest.path()).unwrap();

        let extracted = dest.path().join("proofs/data.mdb");
        assert!(extracted.exists(), "extracted file should exist at proofs/data.mdb");
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

        extract_tar_zst(&archive_path, dest.path()).unwrap();

        assert!(dest.path().join("proofs/data.mdb").exists(), "data.mdb should exist");
        assert!(dest.path().join("proofs/lock.mdb").exists(), "lock.mdb should exist");
        assert!(dest.path().join("proofs/nested/deep.dat").exists(), "nested file should exist");
        assert_eq!(std::fs::read(dest.path().join("proofs/nested/deep.dat")).unwrap(), b"deep");
    }

    #[test]
    fn extract_tar_zst_fails_on_missing_archive() {
        let dest = tempfile::tempdir().unwrap();
        let result = extract_tar_zst(&dest.path().join("nonexistent.tar.zst"), dest.path());
        assert!(result.is_err(), "missing archive should fail");
    }
}
