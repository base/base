//! Download planning: resolves a manifest and preset into concrete archives.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::{debug, info};

use crate::{OutputFileChecksum, SnapshotManifest};

/// Components available in a Base snapshot.
///
/// Unlike reth's fixed `SnapshotComponentType`, this supports the `Proofs`
/// variant for Base's expanded trie proof database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DownloadComponent {
    /// MDBX state database.
    State,
    /// Block headers (chunked).
    Headers,
    /// Transactions (chunked).
    Transactions,
    /// Precomputed transaction senders (chunked).
    TransactionSenders,
    /// Transaction receipts (chunked).
    Receipts,
    /// Account state change history (chunked).
    AccountChangesets,
    /// Storage slot change history (chunked).
    StorageChangesets,
    /// `RocksDB` secondary indices.
    RocksdbIndices,
    /// Expanded trie proof database for fault proofs.
    Proofs,
}

impl DownloadComponent {
    /// The JSON key used in the manifest's `components` map.
    pub const fn manifest_key(&self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Headers => "headers",
            Self::Transactions => "transactions",
            Self::TransactionSenders => "transaction_senders",
            Self::Receipts => "receipts",
            Self::AccountChangesets => "account_changesets",
            Self::StorageChangesets => "storage_changesets",
            Self::RocksdbIndices => "rocksdb_indices",
            Self::Proofs => "proofs",
        }
    }

    /// Whether this component uses chunked archives (multiple files by block range).
    pub const fn is_chunked(&self) -> bool {
        matches!(
            self,
            Self::Headers
                | Self::Transactions
                | Self::TransactionSenders
                | Self::Receipts
                | Self::AccountChangesets
                | Self::StorageChangesets
        )
    }
}

/// Preset selections that map to sets of components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SelectionPreset {
    /// State + headers only.
    Minimal,
    /// State + headers + transactions + receipts.
    Full,
    /// Everything except proofs.
    Archive,
    /// Everything including proofs (archive node + fault proof support).
    Proofs,
}

impl SelectionPreset {
    /// Returns the components included in this preset.
    pub fn components(&self) -> Vec<DownloadComponent> {
        match self {
            Self::Minimal => vec![DownloadComponent::State, DownloadComponent::Headers],
            Self::Full => vec![
                DownloadComponent::State,
                DownloadComponent::Headers,
                DownloadComponent::Transactions,
                DownloadComponent::Receipts,
            ],
            Self::Archive => vec![
                DownloadComponent::State,
                DownloadComponent::Headers,
                DownloadComponent::Transactions,
                DownloadComponent::TransactionSenders,
                DownloadComponent::Receipts,
                DownloadComponent::AccountChangesets,
                DownloadComponent::StorageChangesets,
                DownloadComponent::RocksdbIndices,
            ],
            Self::Proofs => {
                let mut all = Self::Archive.components();
                all.push(DownloadComponent::Proofs);
                all
            }
        }
    }
}

/// A single archive (non-chunked component like state or proofs).
#[derive(Debug, Clone, Deserialize)]
pub struct SingleComponentManifest {
    /// Archive filename (e.g. `state.tar.zst`).
    pub file: String,
    /// Compressed archive size in bytes.
    pub size: u64,
    /// Total decompressed size in bytes.
    pub decompressed_size: u64,
    /// Per-file checksums for verification after extraction.
    pub output_files: Vec<OutputFileChecksum>,
}

/// A chunked archive component (headers, transactions, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct ChunkedComponentManifest {
    /// Blocks per chunk file.
    pub blocks_per_file: u64,
    /// Total blocks covered.
    pub total_blocks: u64,
    /// Compressed size of each chunk archive.
    pub chunk_sizes: Vec<u64>,
    /// Decompressed size of each chunk.
    pub chunk_decompressed_sizes: Vec<u64>,
    /// Per-file checksums for each chunk.
    pub chunk_output_files: Vec<Vec<OutputFileChecksum>>,
    /// Whether each chunk was skipped during snapshot generation
    /// (the chunk already exists in the remote `static_files/` directory).
    #[serde(default)]
    pub chunk_skipped: Vec<bool>,
}

/// A concrete archive to download.
#[derive(Debug, Clone)]
pub struct PlannedArchive {
    /// Which component this archive belongs to.
    pub component: DownloadComponent,
    /// Full download URL.
    pub url: String,
    /// Archive filename for local storage.
    pub file_name: String,
    /// Compressed size in bytes.
    pub size: u64,
    /// Per-file checksums for post-extraction verification.
    pub output_files: Vec<OutputFileChecksum>,
}

/// The result of download planning: all archives to download with totals.
#[derive(Debug)]
pub struct PlannedDownloads {
    /// Ordered list of archives to download.
    pub archives: Vec<PlannedArchive>,
    /// Total compressed bytes to download.
    pub total_download_size: u64,
    /// Number of archives.
    pub total_archives: usize,
}

/// Resolves a manifest and preset into concrete download plans.
#[derive(Debug)]
pub struct DownloadPlanner;

impl DownloadPlanner {
    /// Creates a download plan from a manifest and preset.
    ///
    /// For each selected component, parses the manifest entry and generates
    /// archive URLs. Chunked components produce one archive per chunk (including
    /// chunks marked as `skipped` which live in the `static_files/` directory).
    pub fn plan(
        manifest: &SnapshotManifest,
        preset: SelectionPreset,
        base_url: &str,
        run_prefix: &str,
        static_files_prefix: &str,
    ) -> Result<PlannedDownloads> {
        let selected = preset.components();
        let mut archives = Vec::new();
        let mut total_download_size = 0u64;

        for component in &selected {
            let key = component.manifest_key();
            let Some(value) = manifest.components.get(key) else {
                if *component == DownloadComponent::Proofs {
                    info!(component = key, "proofs component not in manifest, skipping");
                    continue;
                }
                bail!("manifest missing required component: {key}");
            };

            if component.is_chunked() {
                let chunked: ChunkedComponentManifest = serde_json::from_value(value.clone())
                    .with_context(|| format!("failed to parse chunked manifest for {key}"))?;

                Self::plan_chunked_component(
                    *component,
                    &chunked,
                    base_url,
                    run_prefix,
                    static_files_prefix,
                    &mut archives,
                    &mut total_download_size,
                )?;
            } else {
                let single: SingleComponentManifest = serde_json::from_value(value.clone())
                    .with_context(|| format!("failed to parse single manifest for {key}"))?;

                let url = format!("{base_url}/{run_prefix}/{}", single.file);
                total_download_size += single.size;

                archives.push(PlannedArchive {
                    component: *component,
                    url,
                    file_name: single.file.clone(),
                    size: single.size,
                    output_files: single.output_files,
                });
            }
        }

        let total_archives = archives.len();
        info!(
            preset = ?preset,
            total_archives,
            total_download_size,
            "download plan ready"
        );

        Ok(PlannedDownloads { archives, total_download_size, total_archives })
    }

    /// Plans downloads for a chunked component.
    ///
    /// Chunks marked as `skipped` in the manifest live in the remote
    /// `static_files/` directory. Non-skipped chunks live in the per-run
    /// directory. Both are included in the plan since the downloader needs
    /// all chunks for a complete dataset.
    fn plan_chunked_component(
        component: DownloadComponent,
        manifest: &ChunkedComponentManifest,
        base_url: &str,
        run_prefix: &str,
        static_files_prefix: &str,
        archives: &mut Vec<PlannedArchive>,
        total_download_size: &mut u64,
    ) -> Result<()> {
        let key = component.manifest_key();
        let num_chunks = manifest.chunk_sizes.len();

        for i in 0..num_chunks {
            let size = manifest.chunk_sizes[i];
            if size == 0 {
                continue;
            }

            let start = i as u64 * manifest.blocks_per_file;
            let end = start + manifest.blocks_per_file - 1;
            let file_name = format!("{key}-{start}-{end}.tar.zst");

            let is_skipped = manifest.chunk_skipped.get(i).copied().unwrap_or(false);

            let url = if is_skipped {
                format!("{base_url}/{static_files_prefix}/{file_name}")
            } else {
                format!("{base_url}/{run_prefix}/{file_name}")
            };

            let output_files = manifest.chunk_output_files.get(i).cloned().unwrap_or_default();

            *total_download_size += size;
            archives.push(PlannedArchive { component, url, file_name, size, output_files });
        }

        debug!(component = key, chunks = num_chunks, "planned chunked component");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn chunked_component_json() -> serde_json::Value {
        serde_json::json!({
            "blocks_per_file": 500000,
            "total_blocks": 500000,
            "chunk_sizes": [100],
            "chunk_decompressed_sizes": [500],
            "chunk_output_files": [[{"path": "sf/data", "size": 500, "blake3": "x"}]],
        })
    }

    fn add_archive_components(manifest: &mut SnapshotManifest) {
        for key in &[
            "transactions",
            "transaction_senders",
            "receipts",
            "account_changesets",
            "storage_changesets",
        ] {
            manifest.components.insert(key.to_string(), chunked_component_json());
        }
        manifest.components.insert(
            "rocksdb_indices".to_string(),
            serde_json::json!({
                "file": "rocksdb_indices.tar.zst",
                "size": 100,
                "decompressed_size": 500,
                "output_files": [],
            }),
        );
    }

    fn test_manifest() -> SnapshotManifest {
        let mut components = BTreeMap::new();

        components.insert(
            "state".to_string(),
            serde_json::json!({
                "file": "state.tar.zst",
                "size": 1000,
                "decompressed_size": 5000,
                "output_files": [{"path": "db/mdbx.dat", "size": 5000, "blake3": "abc"}],
            }),
        );

        components.insert(
            "headers".to_string(),
            serde_json::json!({
                "blocks_per_file": 500000,
                "total_blocks": 1000000,
                "chunk_sizes": [200, 300],
                "chunk_decompressed_sizes": [1000, 1500],
                "chunk_output_files": [
                    [{"path": "static_files/headers_0_499999", "size": 1000, "blake3": "h0"}],
                    [{"path": "static_files/headers_500000_999999", "size": 1500, "blake3": "h1"}],
                ],
                "chunk_skipped": [true, false],
            }),
        );

        SnapshotManifest {
            block: 1_000_000,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 1700000000,
            components,
        }
    }

    #[test]
    fn plan_minimal_preset() {
        let manifest = test_manifest();
        let plan = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Minimal,
            "https://r2.example.com",
            "mainnet/1700000000",
            "mainnet/static_files",
        )
        .unwrap();

        assert_eq!(plan.total_archives, 3, "state + 2 header chunks");
        assert_eq!(plan.total_download_size, 1500, "1000 + 200 + 300");
    }

    #[test]
    fn skipped_chunks_use_static_files_url() {
        let manifest = test_manifest();
        let plan = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Minimal,
            "https://r2.example.com",
            "mainnet/1700000000",
            "mainnet/static_files",
        )
        .unwrap();

        let chunk0 =
            plan.archives.iter().find(|a| a.file_name == "headers-0-499999.tar.zst").unwrap();
        assert!(
            chunk0.url.contains("static_files"),
            "skipped chunk should use static_files URL: {}",
            chunk0.url
        );

        let chunk1 =
            plan.archives.iter().find(|a| a.file_name == "headers-500000-999999.tar.zst").unwrap();
        assert!(
            chunk1.url.contains("1700000000"),
            "non-skipped chunk should use run prefix URL: {}",
            chunk1.url
        );
    }

    #[test]
    fn proofs_preset_includes_all_plus_proofs() {
        let mut manifest = test_manifest();
        manifest.components.insert(
            "proofs".to_string(),
            serde_json::json!({
                "file": "proofs.tar.zst",
                "size": 2000,
                "decompressed_size": 10000,
                "output_files": [{"path": "proofs/data.dat", "size": 10000, "blake3": "p1"}],
            }),
        );

        add_archive_components(&mut manifest);

        let plan = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Proofs,
            "https://r2.example.com",
            "mainnet/1700000000",
            "mainnet/static_files",
        )
        .unwrap();

        let proofs = plan.archives.iter().find(|a| a.file_name == "proofs.tar.zst");
        assert!(proofs.is_some(), "proofs preset should include proofs archive");
    }

    #[test]
    fn missing_proofs_component_is_skipped_gracefully() {
        let mut manifest = test_manifest();
        add_archive_components(&mut manifest);

        let plan = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Proofs,
            "https://r2.example.com",
            "mainnet/1700000000",
            "mainnet/static_files",
        )
        .unwrap();

        let proofs = plan.archives.iter().find(|a| a.file_name == "proofs.tar.zst");
        assert!(proofs.is_none(), "missing proofs should be gracefully skipped");
    }

    #[test]
    fn missing_required_component_fails() {
        let manifest = SnapshotManifest {
            block: 100,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 1700000000,
            components: BTreeMap::new(),
        };

        let result = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Minimal,
            "https://r2.example.com",
            "prefix/ts",
            "prefix/static_files",
        );

        assert!(result.is_err(), "missing required component should fail");
    }

    #[test]
    fn zero_size_chunks_are_skipped() {
        let mut manifest = test_manifest();
        manifest.components.insert(
            "headers".to_string(),
            serde_json::json!({
                "blocks_per_file": 500000,
                "total_blocks": 1500000,
                "chunk_sizes": [200, 0, 300],
                "chunk_decompressed_sizes": [1000, 0, 1500],
                "chunk_output_files": [
                    [{"path": "a", "size": 1, "blake3": "x"}],
                    [],
                    [{"path": "b", "size": 1, "blake3": "y"}],
                ],
            }),
        );

        let plan = DownloadPlanner::plan(
            &manifest,
            SelectionPreset::Minimal,
            "https://r2.example.com",
            "p/ts",
            "p/sf",
        )
        .unwrap();

        let header_archives: Vec<_> =
            plan.archives.iter().filter(|a| a.component == DownloadComponent::Headers).collect();
        assert_eq!(header_archives.len(), 2, "zero-size chunk should be skipped");
    }
}
