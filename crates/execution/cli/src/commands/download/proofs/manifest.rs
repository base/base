//! Discovery and parsing of the `proofs` component of a snapshot manifest.

use std::{path::Path, time::Duration};

use base_reth_cli::OutputFileChecksum;
use eyre::Result;
use tracing::info;

/// Metadata parsed from the manifest's `proofs` component.
#[derive(Debug, Clone)]
pub struct ProofsManifestEntry {
    /// Archive file name, a single path component.
    pub file_name: String,
    /// Compressed archive size in bytes.
    pub expected_size: u64,
    /// URL of the archive, next to the manifest.
    pub archive_url: String,
    /// Files the archive must extract to, relative to the target datadir.
    pub output_files: Vec<OutputFileChecksum>,
}

/// Locates and reads the proofs component of a snapshot manifest.
#[derive(Debug)]
pub struct ProofsManifest;

impl ProofsManifest {
    /// Discovers the latest modular snapshot manifest URL for `chain_id`.
    ///
    /// Reth's download pipeline queries the snapshot API (`metadataUrl`) rather than
    /// concatenating `{default_base_url}/{chain_id}/manifest.json`. Each chain publishes
    /// manifests under its own bucket (`https://zeronet-v2-snapshots.base.org/...`), so
    /// deriving a URL from the snapshot root instead produces 404s.
    pub async fn discover_latest_url(api_url: &str, chain_id: u64) -> Result<String> {
        info!(
            target: "reth::cli",
            api_url = %api_url,
            chain_id,
            "Discovering latest snapshot manifest for proofs"
        );

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .build()?;

        let listing: serde_json::Value = client
            .get(api_url)
            .send()
            .await
            .map_err(|e| eyre::eyre!("failed to fetch snapshot listing from {api_url}: {e}"))?
            .error_for_status()
            .map_err(|e| eyre::eyre!("failed to fetch snapshot listing from {api_url}: {e}"))?
            .json()
            .await
            .map_err(|e| eyre::eyre!("failed to parse snapshot listing from {api_url}: {e}"))?;

        let entries = listing
            .as_array()
            .ok_or_else(|| eyre::eyre!("snapshot listing from {api_url} is not a JSON array"))?;

        let (block, metadata_url) = entries
            .iter()
            .filter_map(|entry| {
                let id = Self::json_u64(entry.get("chainId"))?;
                if id != chain_id {
                    return None;
                }
                let metadata_url = entry.get("metadataUrl").and_then(|v| v.as_str())?;
                if !metadata_url.ends_with("manifest.json") {
                    return None;
                }
                let block = Self::json_u64(entry.get("block"))?;
                Some((block, metadata_url.to_string()))
            })
            .max_by_key(|(block, _)| *block)
            .ok_or_else(|| {
                eyre::eyre!("no modular snapshot manifest found for chain {chain_id} at {api_url}")
            })?;

        info!(
            target: "reth::cli",
            block,
            url = %metadata_url,
            "Found latest snapshot manifest for proofs"
        );

        Ok(metadata_url)
    }

    /// Fetches the manifest and extracts the proofs component metadata.
    pub async fn fetch_entry(manifest_url: &str) -> Result<ProofsManifestEntry> {
        info!(target: "reth::cli", manifest_url = %manifest_url, "Fetching manifest for proofs component");

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
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

        let file_path = Path::new(&file_name);
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

        let output_files: Vec<OutputFileChecksum> = match proofs_component.get("output_files") {
            Some(files) => serde_json::from_value(files.clone()).map_err(|e| {
                eyre::eyre!("invalid 'output_files' for proofs component in manifest: {e}")
            })?,
            None => Vec::new(),
        };
        if let Some(file) = output_files.iter().find(|file| {
            file.path.is_empty()
                || Path::new(&file.path)
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
        }) {
            eyre::bail!("invalid proofs output file path in manifest: {}", file.path);
        }

        Ok(ProofsManifestEntry { file_name, expected_size, archive_url, output_files })
    }

    /// Reads a snapshot API field that may be a JSON number or a numeric string.
    fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
        match value {
            Some(serde_json::Value::Number(n)) => n.as_u64(),
            Some(serde_json::Value::String(s)) => s.parse().ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, routing::get};

    use super::*;
    use crate::commands::download::proofs::test_utils::{
        create_proofs_archive, serve, start_test_server,
    };

    async fn start_snapshot_api_server(
        listing: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listing_bytes = serde_json::to_vec(&listing).unwrap();
        let app = Router::new().route(
            "/api/snapshots",
            get(move || {
                let data = listing_bytes.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], data) }
            }),
        );

        let (base_url, handle) = serve(app).await;
        (format!("{base_url}/api/snapshots"), handle)
    }

    #[tokio::test]
    async fn discover_latest_url_picks_latest_modular_for_chain() {
        let listing = serde_json::json!([
            {
                "chainId": "8453",
                "block": "100",
                "metadataUrl": "https://mainnet-v2-snapshots.base.org/old/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "10",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/old/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "20",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/new/manifest.json"
            },
            {
                "chainId": "763360",
                "block": "15",
                "metadataUrl": "https://zeronet-v2-snapshots.base.org/not-a-manifest.tar.zst"
            }
        ]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let url = ProofsManifest::discover_latest_url(&api_url, 763360).await.unwrap();

        assert_eq!(
            url, "https://zeronet-v2-snapshots.base.org/new/manifest.json",
            "proofs must use the latest modular metadataUrl for the requested chain"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn discover_latest_url_accepts_numeric_ids() {
        let listing = serde_json::json!([{
            "chainId": 763360,
            "block": 3584106,
            "metadataUrl": "https://zeronet-v2-snapshots.base.org/1789516802/manifest.json"
        }]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let url = ProofsManifest::discover_latest_url(&api_url, 763360).await.unwrap();

        assert_eq!(
            url, "https://zeronet-v2-snapshots.base.org/1789516802/manifest.json",
            "snapshot API numeric chainId/block fields should be accepted"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn discover_latest_url_fails_when_chain_missing() {
        let listing = serde_json::json!([{
            "chainId": "8453",
            "block": "100",
            "metadataUrl": "https://mainnet-v2-snapshots.base.org/old/manifest.json"
        }]);

        let (api_url, handle) = start_snapshot_api_server(listing).await;
        let result = ProofsManifest::discover_latest_url(&api_url, 763360).await;

        assert!(result.is_err(), "missing chain should fail discovery");
        assert!(
            result.unwrap_err().to_string().contains("no modular snapshot manifest"),
            "error should name the missing modular snapshot"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_entry_extracts_proofs_metadata() {
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
        let entry = ProofsManifest::fetch_entry(&manifest_url).await.unwrap();

        assert_eq!(entry.file_name, "proofs.tar.zst");
        assert_eq!(entry.expected_size, archive.len() as u64);
        assert!(entry.archive_url.ends_with("/proofs.tar.zst"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_entry_rejects_path_traversal() {
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
        let result = ProofsManifest::fetch_entry(&manifest_url).await;

        assert!(result.is_err(), "path traversal should be rejected");
        assert!(result.unwrap_err().to_string().contains("invalid proofs file name"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_entry_fails_when_no_proofs() {
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
        let result = ProofsManifest::fetch_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no 'proofs' component"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_entry_fails_when_size_missing() {
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
        let result = ProofsManifest::fetch_entry(&manifest_url).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'size'"));

        handle.abort();
    }

    #[tokio::test]
    async fn fetch_entry_rejects_output_file_traversal() {
        for path in ["../escape", "/etc/passwd", "proofs/../../escape", ""] {
            let manifest = serde_json::json!({
                "block": 100,
                "chain_id": 8453,
                "storage_version": 2,
                "timestamp": 1700000000,
                "components": {
                    "proofs": {
                        "file": "proofs.tar.zst",
                        "size": 100,
                        "decompressed_size": 0,
                        "output_files": [{ "path": path, "size": 1, "blake3": "00" }]
                    }
                }
            });

            let (manifest_url, handle) = start_test_server(manifest, vec![]).await;
            let error = ProofsManifest::fetch_entry(&manifest_url)
                .await
                .expect_err("unsafe output path should be rejected");

            assert!(
                error.to_string().contains("invalid proofs output file path"),
                "path {path:?}: {error}"
            );

            handle.abort();
        }
    }
}
