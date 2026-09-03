//! Download command wrapper that extends reth's `DownloadCommand` with `--proofs`.
//!
//! Delegates all standard snapshot components to reth's download pipeline and
//! handles the Base-specific proofs database download separately using the
//! same snapshot source and manifest.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use base_execution_chainspec::BaseChainSpec;
use clap::Parser;
use eyre::Result;
use futures::StreamExt;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::download::{
    DownloadCommand, DownloadDefaults,
    manifest::{ComponentManifest, OutputFileChecksum, SingleArchive},
};
use reth_node_core::args::DatadirArgs;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::info;
use url::Url;

/// Download Base node snapshots from R2 storage.
///
/// Wraps reth's download command with an additional `--proofs` flag that
/// downloads the expanded trie proof database for fault proof support.
///
/// When `--proofs` is passed, the command runs reth's standard download
/// then fetches and extracts incremental `RocksDB` proofs artifacts from the
/// same snapshot source.
#[derive(Debug, Parser)]
pub struct BaseDownloadCommand<C: ChainSpecParser> {
    #[command(flatten)]
    inner: DownloadCommand<C>,

    /// Also download the proofs database for fault proof support.
    ///
    /// After the standard download completes, fetches mutable metadata and
    /// immutable SST archives from the same snapshot source. Existing SSTs
    /// with matching BLAKE3 metadata are reused.
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
            let dir = resolve_datadir_args(std::env::args_os()).resolve_datadir(chain);
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

/// A Base-specific top-level manifest extension containing immutable proof SST tables.
#[derive(Debug, Deserialize)]
struct ProofsStaticManifest {
    version: u8,
    database: String,
    tables: Vec<SingleArchive>,
}

#[derive(Debug, Deserialize)]
struct ProofsDownloadManifest {
    #[serde(default)]
    base_url: Option<String>,
    components: BTreeMap<String, ComponentManifest>,
    #[serde(default)]
    proofs_static: Option<ProofsStaticManifest>,
}

/// A concrete proof artifact to fetch and verify.
#[derive(Debug)]
struct ProofsManifestEntry {
    file_name: String,
    expected_size: u64,
    archive_url: String,
    output_files: Vec<OutputFileChecksum>,
}

/// Downloads the proofs database from a snapshot manifest.
///
/// Encapsulates the full pipeline: manifest fetch → artifact reuse or download
/// with resume → tar+zstd extraction → BLAKE3 output verification.
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
        let entries = Self::fetch_manifest_entries(manifest_url).await?;

        let cache_dir = target_dir.join(".snapshot-cache");
        tokio::fs::create_dir_all(&cache_dir).await?;

        for entry in entries {
            if Self::verify_outputs(target_dir, &entry.output_files)? {
                info!(target: "reth::cli", file = %entry.file_name, "Reusing verified proofs snapshot artifact");
                continue;
            }

            Self::cleanup_outputs(target_dir, &entry.output_files);
            let archive_path = Self::download_archive(&entry, &cache_dir).await?;
            Self::extract_tar_zst(&archive_path, target_dir)?;
            tokio::fs::remove_file(&archive_path).await.ok();

            if !Self::verify_outputs(target_dir, &entry.output_files)? {
                Self::cleanup_outputs(target_dir, &entry.output_files);
                eyre::bail!(
                    "proofs archive extracted but output verification failed: {}",
                    entry.file_name
                );
            }
        }

        tokio::fs::remove_dir_all(cache_dir).await.ok();
        info!(target: "reth::cli", "Proofs database download complete");
        Ok(())
    }

    /// Fetches the manifest and resolves immutable SST tables plus mutable metadata.
    async fn fetch_manifest_entries(manifest_url: &str) -> Result<Vec<ProofsManifestEntry>> {
        info!(target: "reth::cli", manifest_url = %manifest_url, "Fetching manifest for proofs components");

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        let manifest: ProofsDownloadManifest = client
            .get(manifest_url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .error_for_status()
            .map_err(|e| eyre::eyre!("failed to fetch manifest from {manifest_url}: {e}"))?
            .json()
            .await
            .map_err(|e| eyre::eyre!("failed to parse manifest from {manifest_url}: {e}"))?;

        let proofs_static = manifest.proofs_static.ok_or_else(|| {
            eyre::eyre!("manifest has no proofs_static extension — this snapshot uses an unsupported proofs format")
        })?;
        if proofs_static.version != 1 || proofs_static.database != "rocksdb" {
            eyre::bail!(
                "unsupported proofs_static format: version={} database={}",
                proofs_static.version,
                proofs_static.database
            );
        }

        let ComponentManifest::Single(metadata) = manifest
            .components
            .get("proofs")
            .ok_or_else(|| eyre::eyre!("manifest has no proofs metadata component"))?
        else {
            eyre::bail!("proofs metadata component must be a single archive");
        };

        let archive_base_url = manifest.base_url.as_deref().unwrap_or_else(|| {
            manifest_url.rsplit_once('/').map(|(base, _)| base).unwrap_or(manifest_url)
        });
        let mut entries = proofs_static
            .tables
            .iter()
            .map(|table| Self::entry_from_archive(table, archive_base_url))
            .collect::<Result<Vec<_>>>()?;
        entries.push(Self::entry_from_archive(metadata, archive_base_url)?);
        Ok(entries)
    }

    fn entry_from_archive(archive: &SingleArchive, base_url: &str) -> Result<ProofsManifestEntry> {
        Self::validate_relative_archive_path(&archive.file)?;
        Self::validate_output_files(&archive.output_files)?;
        let mut base = Url::parse(base_url)
            .map_err(|e| eyre::eyre!("invalid proofs archive base URL {base_url}: {e}"))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let archive_url = base
            .join(&archive.file)
            .map_err(|e| eyre::eyre!("invalid proofs archive URL {}: {e}", archive.file))?
            .to_string();
        let file_name = format!("{}.tar.zst", blake3::hash(archive.file.as_bytes()).to_hex());
        Ok(ProofsManifestEntry {
            file_name,
            expected_size: archive.size,
            archive_url,
            output_files: archive.output_files.clone(),
        })
    }

    fn validate_relative_archive_path(file: &str) -> Result<()> {
        let path = Path::new(file);
        if file.is_empty()
            || path.is_absolute()
            || !file.ends_with(".tar.zst")
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            eyre::bail!("invalid proofs archive path in manifest: {file}");
        }
        Ok(())
    }

    fn validate_output_files(files: &[OutputFileChecksum]) -> Result<()> {
        if files.is_empty() {
            eyre::bail!("proofs archive is missing output checksum metadata");
        }
        for file in files {
            let path = Path::new(&file.path);
            if !file.path.starts_with("proofs/")
                || path.is_absolute()
                || path
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                eyre::bail!("invalid proofs output path in manifest: {}", file.path);
            }
        }
        Ok(())
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

    fn verify_outputs(target_dir: &Path, output_files: &[OutputFileChecksum]) -> Result<bool> {
        for expected in output_files {
            let path = target_dir.join(&expected.path);
            let Ok(metadata) = std::fs::metadata(&path) else { return Ok(false) };
            if metadata.len() != expected.size {
                return Ok(false);
            }
            let mut source = std::fs::File::open(path)?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = source.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            if !hasher.finalize().to_hex().eq_ignore_ascii_case(&expected.blake3) {
                return Ok(false);
            }
        }
        Ok(!output_files.is_empty())
    }

    fn cleanup_outputs(target_dir: &Path, output_files: &[OutputFileChecksum]) {
        for output in output_files {
            let _ = std::fs::remove_file(target_dir.join(&output.path));
        }
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
    use std::{collections::HashMap, path::Path};

    use axum::{
        Router,
        extract::{Path as AxumPath, State},
        http::StatusCode,
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
        builder.into_inner().unwrap().finish().unwrap();
        buf
    }

    fn output(path: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "path": path,
            "size": bytes.len(),
            "blake3": blake3::hash(bytes).to_hex().to_string(),
        })
    }

    async fn start_test_server(
        manifest: serde_json::Value,
        archives: HashMap<String, Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        async fn archive_handler(
            AxumPath(path): AxumPath<String>,
            State(archives): State<HashMap<String, Vec<u8>>>,
        ) -> impl IntoResponse {
            archives.get(&path).map_or_else(
                || StatusCode::NOT_FOUND.into_response(),
                |data| (StatusCode::OK, data.clone()).into_response(),
            )
        }

        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let app = Router::new()
            .route(
                "/manifest.json",
                get(move || {
                    let data = manifest_bytes.clone();
                    async move { data }
                }),
            )
            .route("/{*path}", get(archive_handler))
            .with_state(archives);
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

    #[test]
    fn resolve_datadir_args_uses_explicit_datadir() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir"),
            OsString::from("/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir without adding the chain directory"
        );
    }

    #[test]
    fn resolve_datadir_args_uses_equals_syntax() {
        let datadir = resolve_datadir_args([
            OsString::from("test"),
            OsString::from("--datadir=/tmp/base-download-test"),
        ])
        .resolve_datadir(BaseChainSpec::mainnet().chain());

        assert_eq!(
            datadir.data_dir(),
            Path::new("/tmp/base-download-test"),
            "proofs download should use --datadir=VALUE without adding the chain directory"
        );
    }
    #[tokio::test]
    async fn fetch_manifest_entries_requires_rocksdb_static_extension() {
        let manifest = serde_json::json!({
            "components": {"proofs": {"file": "1/proofs-metadata.tar.zst", "size": 1, "output_files": []}}
        });
        let (url, handle) = start_test_server(manifest, HashMap::new()).await;
        let error = ProofsDownloader::fetch_manifest_entries(&url).await.unwrap_err();
        assert!(error.to_string().contains("proofs_static"));
        handle.abort();
    }

    #[tokio::test]
    async fn downloads_metadata_and_static_tables_and_reuses_verified_sst() {
        let table = create_proofs_archive(&[("proofs/000001.sst", b"table")]);
        let metadata = create_proofs_archive(&[
            ("proofs/CURRENT", b"MANIFEST-000001\n"),
            ("proofs/MANIFEST-000001", b"manifest"),
        ]);
        let manifest = serde_json::json!({
            "components": {
                "proofs": {
                    "file": "1/proofs-metadata.tar.zst",
                    "size": metadata.len(),
                    "decompressed_size": 0,
                    "output_files": [
                        output("proofs/CURRENT", b"MANIFEST-000001\n"),
                        output("proofs/MANIFEST-000001", b"manifest")
                    ]
                }
            },
            "proofs_static": {
                "version": 1,
                "database": "rocksdb",
                "tables": [{
                    "file": "static_files/proofs/table.tar.zst",
                    "size": table.len(),
                    "decompressed_size": 0,
                    "output_files": [output("proofs/000001.sst", b"table")]
                }]
            }
        });
        let mut archives = HashMap::new();
        archives.insert("static_files/proofs/table.tar.zst".to_string(), table);
        archives.insert("1/proofs-metadata.tar.zst".to_string(), metadata);
        let target = tempfile::tempdir().unwrap();
        let (url, handle) = start_test_server(manifest, archives).await;
        ProofsDownloader::run_from_manifest(target.path(), &url).await.unwrap();
        assert_eq!(std::fs::read(target.path().join("proofs/000001.sst")).unwrap(), b"table");
        ProofsDownloader::run_from_manifest(target.path(), &url).await.unwrap();
        handle.abort();
    }

    #[tokio::test]
    async fn generated_fake_rocksdb_proofs_snapshot_restores_end_to_end() {
        use base_snapshotter::{
            ManifestGenerationParams, ProofsStaticManifest, SnapshotGenerator, SnapshotManifest,
            SnapshotUploadParams, SnapshotUploader,
        };
        use testcontainers::runners::AsyncRunner;
        use testcontainers_modules::minio::MinIO;

        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("db")).unwrap();
        std::fs::write(source.path().join("db/mdbx.dat"), b"state").unwrap();

        let proofs = source.path().join("proofs");
        std::fs::create_dir_all(&proofs).unwrap();
        std::fs::write(proofs.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
        std::fs::write(proofs.join("MANIFEST-000001"), b"manifest").unwrap();
        std::fs::write(proofs.join("000001.sst"), b"immutable-table").unwrap();

        let generated = tempfile::tempdir().unwrap();
        let remote_static_files = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&ManifestGenerationParams {
            source_datadir: source.path(),
            output_dir: generated.path(),
            chain_id: 8453,
            base_url: None,
            block: Some(0),
            blocks_per_file: Some(500_000),
            remote_static_files: &remote_static_files,
            previous_manifest: None,
            upload_proofs: true,
        })
        .unwrap();

        let manifest_bytes = std::fs::read(generated.path().join("manifest.json")).unwrap();
        let local_manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let proofs_static = ProofsStaticManifest::from_manifest_bytes(&manifest_bytes)
            .unwrap()
            .expect("proofs_static extension");

        let minio = MinIO::default().start().await.unwrap();
        let endpoint =
            format!("http://127.0.0.1:{}", minio.get_host_port_ipv4(9000).await.unwrap());
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region("us-east-1")
            .endpoint_url(endpoint)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "minioadmin",
                "minioadmin",
                None,
                None,
                "test",
            ))
            .load()
            .await;
        let client = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&config).force_path_style(true).build(),
        );
        let bucket = format!("proofs-e2e-{}", std::process::id());
        client.create_bucket().bucket(&bucket).send().await.unwrap();

        let uploader =
            SnapshotUploader::new(client.clone(), bucket.clone(), "proofs-e2e".into(), None);
        uploader
            .upload(SnapshotUploadParams {
                output_dir: generated.path(),
                files: &files,
                timestamp: 42,
                retain_runs: 1,
                local_manifest: &local_manifest,
                remote_manifest: None,
                remote_static_files: &remote_static_files,
                proofs_static: Some(&proofs_static),
                rocksdb_static: None,
            })
            .await
            .unwrap();

        async fn fetch_object(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Vec<u8> {
            client
                .get_object()
                .bucket(bucket)
                .key(key)
                .send()
                .await
                .unwrap()
                .body
                .collect()
                .await
                .unwrap()
                .into_bytes()
                .to_vec()
        }

        let published_manifest =
            fetch_object(&client, &bucket, "proofs-e2e/42/manifest.json").await;
        let manifest: serde_json::Value = serde_json::from_slice(&published_manifest).unwrap();
        let table_path =
            manifest["proofs_static"]["tables"][0]["file"].as_str().unwrap().to_string();
        let metadata_path = manifest["components"]["proofs"]["file"].as_str().unwrap().to_string();
        let mut archives = HashMap::new();
        archives.insert(
            table_path.clone(),
            fetch_object(&client, &bucket, &format!("proofs-e2e/{table_path}")).await,
        );
        archives.insert(
            metadata_path.clone(),
            fetch_object(&client, &bucket, &format!("proofs-e2e/{metadata_path}")).await,
        );

        let target = tempfile::tempdir().unwrap();
        let (url, handle) = start_test_server(manifest, archives).await;
        ProofsDownloader::run_from_manifest(target.path(), &url).await.unwrap();

        assert_eq!(
            std::fs::read(target.path().join("proofs/000001.sst")).unwrap(),
            b"immutable-table"
        );
        assert_eq!(
            std::fs::read(target.path().join("proofs/CURRENT")).unwrap(),
            b"MANIFEST-000001\n"
        );
        assert_eq!(
            std::fs::read(target.path().join("proofs/MANIFEST-000001")).unwrap(),
            b"manifest"
        );

        handle.abort();
    }

    #[test]
    fn rejects_archive_or_output_path_traversal() {
        assert!(ProofsDownloader::validate_relative_archive_path("../proofs.tar.zst").is_err());
        let files =
            vec![OutputFileChecksum { path: "../bad".to_string(), size: 0, blake3: String::new() }];
        assert!(ProofsDownloader::validate_output_files(&files).is_err());
    }
}
