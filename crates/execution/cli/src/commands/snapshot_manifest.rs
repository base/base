//! Base-owned snapshot manifest command.

use std::{collections::HashMap, path::PathBuf};

use base_reth_cli::{ManifestGenerationParams, SnapshotGenerator, SnapshotManifest};
use clap::Parser;
use eyre::{ContextCompat, Result, WrapErr};
use reth_db::{Database, mdbx::DatabaseArguments, open_db_read_only, tables};
use reth_db_api::transaction::DbTx;
use reth_stages_types::StageId;
use tracing::{info, warn};

/// Generate modular chunk archives and a snapshot manifest from a source datadir.
#[derive(Debug, Parser)]
pub struct SnapshotManifestCommand {
    /// Source datadir containing static files and databases.
    #[arg(long, short = 'd')]
    source_datadir: PathBuf,

    /// Output directory where new chunk archives and manifest.json are written.
    #[arg(long, short = 'o')]
    output_dir: PathBuf,

    /// Optional base URL where archives will be hosted.
    #[arg(long)]
    base_url: Option<String>,

    /// Block number this snapshot was taken at.
    ///
    /// If omitted, this is inferred from the source datadir's Finish stage checkpoint.
    #[arg(long)]
    block: Option<u64>,

    /// Chain ID.
    #[arg(long, default_value = "1")]
    chain_id: u64,

    /// Blocks per archive file for chunked components.
    ///
    /// If omitted, this is inferred from header static-file ranges in the source datadir.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    blocks_per_file: Option<u64>,

    /// Previous manifest whose uncompressed file hashes may be reused.
    #[arg(long, requires = "existing_archives")]
    previous_manifest: Option<PathBuf>,

    /// JSON object mapping existing archive filenames to their compressed sizes.
    #[arg(long, requires = "previous_manifest")]
    existing_archives: Option<PathBuf>,

    /// Include the proofs database in the snapshot.
    #[arg(long)]
    proofs: bool,
}

impl SnapshotManifestCommand {
    /// Packages snapshot archives and writes the manifest file.
    pub fn execute(self) -> Result<()> {
        let block = match self.block {
            Some(block) => block,
            None => infer_snapshot_block(&self.source_datadir)?,
        };
        let blocks_per_file = match self.blocks_per_file {
            Some(blocks_per_file) => blocks_per_file,
            None => infer_blocks_per_file(&self.source_datadir)?,
        };
        let remote_static_files = read_existing_archives(self.existing_archives.as_ref())?;
        let previous_manifest = read_previous_manifest(self.previous_manifest.as_ref())?;
        info!(
            source = %self.source_datadir.display(),
            output = %self.output_dir.display(),
            block,
            "packaging modular snapshot archives"
        );
        SnapshotGenerator::generate_manifest(&ManifestGenerationParams {
            source_datadir: &self.source_datadir,
            output_dir: &self.output_dir,
            chain_id: self.chain_id,
            base_url: self.base_url.as_deref(),
            block: Some(block),
            blocks_per_file: Some(blocks_per_file),
            remote_static_files: &remote_static_files,
            previous_manifest: previous_manifest.as_ref(),
            upload_proofs: self.proofs,
        })
        .map_err(|error| eyre::eyre!("snapshot generation failed: {error:#}"))?;
        Ok(())
    }
}

fn read_existing_archives(path: Option<&PathBuf>) -> Result<HashMap<String, u64>> {
    let Some(path) = path else { return Ok(HashMap::new()) };
    let bytes = std::fs::read(path)
        .wrap_err_with(|| format!("failed to read existing archives from {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("failed to parse existing archives from {}", path.display()))
}

fn read_previous_manifest(path: Option<&PathBuf>) -> Result<Option<SnapshotManifest>> {
    let Some(path) = path else { return Ok(None) };
    let bytes = std::fs::read(path)
        .wrap_err_with(|| format!("failed to read previous manifest from {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("failed to parse previous manifest from {}", path.display()))
        .map(Some)
}

fn infer_snapshot_block(source_datadir: &std::path::Path) -> Result<u64> {
    if let Ok(block) = infer_snapshot_block_from_db(source_datadir) {
        return Ok(block);
    }

    warn!("could not read Finish stage checkpoint; using static-file tip");
    infer_snapshot_block_from_headers(source_datadir)
}

fn infer_snapshot_block_from_db(source_datadir: &std::path::Path) -> Result<u64> {
    for db_path in [source_datadir.join("db"), source_datadir.to_path_buf()] {
        if !db_path.exists() {
            continue;
        }
        let Ok(db) = open_db_read_only(&db_path, DatabaseArguments::default()) else { continue };
        let tx = db.tx()?;
        if let Some(checkpoint) = tx.get::<tables::StageCheckpoints>(StageId::Finish.to_string())? {
            return Ok(checkpoint.block_number);
        }
    }
    eyre::bail!("could not infer block from the Finish stage checkpoint")
}

fn infer_snapshot_block_from_headers(source_datadir: &std::path::Path) -> Result<u64> {
    header_ranges(source_datadir)?
        .into_iter()
        .map(|(_, end)| end)
        .max()
        .context("no header static files found to infer block")
}

fn infer_blocks_per_file(source_datadir: &std::path::Path) -> Result<u64> {
    let mut inferred = None;
    for (start, end) in header_ranges(source_datadir)? {
        let span = end.saturating_sub(start).saturating_add(1);
        if let Some(existing) = inferred {
            if existing != span {
                eyre::bail!("inconsistent header static-file ranges; pass --blocks-per-file")
            }
        } else {
            inferred = Some(span);
        }
    }
    inferred.context("could not infer --blocks-per-file; pass it manually")
}

fn header_ranges(source_datadir: &std::path::Path) -> Result<Vec<(u64, u64)>> {
    let static_files_dir = source_datadir.join("static_files");
    let static_files_dir =
        if static_files_dir.exists() { static_files_dir } else { source_datadir.to_path_buf() };
    let ranges = std::fs::read_dir(&static_files_dir)
        .wrap_err_with(|| format!("failed to read {}", static_files_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| parse_headers_range(&entry.file_name().to_string_lossy()))
        .collect();
    Ok(ranges)
}

fn parse_headers_range(file_name: &str) -> Option<(u64, u64)> {
    let remainder = file_name.strip_prefix("static_file_headers_")?;
    let (start, end_with_suffix) = remainder.split_once('_')?;
    let end = end_with_suffix.chars().take_while(char::is_ascii_digit).collect::<String>();
    Some((start.parse().ok()?, end.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_ranges_with_suffixes() {
        assert_eq!(parse_headers_range("static_file_headers_0_499999.off"), Some((0, 499_999)));
        assert_eq!(parse_headers_range("static_file_transactions_0_499999"), None);
    }
}
