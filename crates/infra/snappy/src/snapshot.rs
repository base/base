//! Snapshot manifest generation wrapping reth's `generate_manifest`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reth_cli_commands::download::manifest::generate_manifest;
use tracing::info;

/// Default blocks per static file segment.
const DEFAULT_BLOCKS_PER_FILE: u64 = 500_000;

/// Generates snapshot archives and a manifest from a reth datadir.
#[derive(Debug)]
pub struct SnapshotGenerator;

impl SnapshotGenerator {
    /// Runs reth's `generate_manifest` to produce `manifest.json` and `*.tar.zst`
    /// archives in `output_dir`.
    ///
    /// `block` must be provided — the caller is responsible for determining the
    /// snapshot block number (e.g. from the node's DB checkpoint or CLI args).
    ///
    /// Returns the list of files created in the output directory.
    pub fn generate(
        source_datadir: &Path,
        output_dir: &Path,
        chain_id: u64,
        block: u64,
        blocks_per_file: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create output dir {}", output_dir.display()))?;

        let blocks_per_file = blocks_per_file.unwrap_or(DEFAULT_BLOCKS_PER_FILE);

        info!(
            source = %source_datadir.display(),
            output = %output_dir.display(),
            chain_id,
            block,
            blocks_per_file,
            "generating snapshot manifest"
        );

        let manifest =
            generate_manifest(source_datadir, output_dir, None, block, chain_id, blocks_per_file)
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;

        let json =
            serde_json::to_string_pretty(&manifest).context("failed to serialize manifest")?;
        let manifest_path = output_dir.join("manifest.json");
        std::fs::write(&manifest_path, &json)
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;

        let num_components = manifest.components.len();
        info!(components = num_components, block = manifest.block, "manifest written");

        let files = collect_output_files(output_dir)?;
        info!(file_count = files.len(), "snapshot generation complete");
        Ok(files)
    }
}

/// Collects all files in the output directory (non-recursive).
fn collect_output_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort_unstable();
    Ok(files)
}
