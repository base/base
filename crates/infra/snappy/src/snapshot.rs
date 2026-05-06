//! Snapshot manifest generation via reth's `SnapshotManifestCommand`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use reth_cli_commands::download::manifest_cmd::SnapshotManifestCommand;
use tracing::info;

/// Generates snapshot archives and a manifest from a reth datadir.
///
/// Delegates to reth's `SnapshotManifestCommand` which handles block number
/// and blocks-per-file inference from the source datadir when not provided.
#[derive(Debug)]
pub struct SnapshotGenerator;

impl SnapshotGenerator {
    /// Runs reth's `SnapshotManifestCommand` to produce `manifest.json` and
    /// `*.tar.zst` archives in `output_dir`.
    ///
    /// Returns the list of files created in the output directory.
    pub fn generate(
        source_datadir: &Path,
        output_dir: &Path,
        chain_id: u64,
        block: Option<u64>,
        blocks_per_file: Option<u64>,
    ) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create output dir {}", output_dir.display()))?;

        let source = source_datadir.to_str().context("source_datadir is not valid UTF-8")?;
        let output = output_dir.to_str().context("output_dir is not valid UTF-8")?;
        let chain_id_str = chain_id.to_string();

        let mut args: Vec<&str> =
            vec!["snapshot-manifest", "-d", source, "-o", output, "--chain-id", &chain_id_str];

        let block_str = block.map(|b| b.to_string());
        if let Some(ref b) = block_str {
            args.extend(["--block", b]);
        }

        let bpf_str = blocks_per_file.map(|b| b.to_string());
        if let Some(ref b) = bpf_str {
            args.extend(["--blocks-per-file", b]);
        }

        info!(
            source = %source_datadir.display(),
            output = %output_dir.display(),
            chain_id,
            block = ?block,
            "generating snapshot manifest"
        );

        let cmd = SnapshotManifestCommand::parse_from(args);
        cmd.execute().map_err(|e| anyhow::anyhow!("{e:#}"))?;

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
