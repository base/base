//! Snapshot manifest command and archive generation with selective compression.
//!
//! Archive creation, BLAKE3 hashing, and manifest structure are derived from
//! [reth](https://github.com/paradigmxyz/reth) (`crates/cli/commands/src/download/manifest.rs`,
//! commit `d58c6e3`, tag `v2.1.0`), licensed under Apache-2.0.
//!
//! Modified to reuse archives whose uncompressed source files match a previous manifest.

use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use humantime::{FormattedDuration, format_duration};
use rayon::prelude::*;
pub use reth_cli_commands::download::manifest::{
    ChunkedArchive, ComponentManifest, OutputFileChecksum, SingleArchive, SnapshotManifest,
};
use tracing::info;

/// Default blocks per static file segment.
const DEFAULT_BLOCKS_PER_FILE: u64 = 500_000;

/// Maximum number of chunks allowed before bailing to prevent OOM.
/// At 500k blocks per file, 100k chunks covers 50 billion blocks.
const MAX_CHUNKS: u64 = 100_000;

/// Interval between progress logs while compressing a database component.
const COMPRESSION_LOG_INTERVAL: Duration = Duration::from_secs(3);

/// Static file component types that produce chunked archives.
const CHUNKED_COMPONENTS: &[(&str, &str)] = &[
    ("headers", "headers"),
    ("transactions", "transactions"),
    ("transaction_senders", "transaction-senders"),
    ("receipts", "receipts"),
    ("account_changesets", "account-change-sets"),
    ("storage_changesets", "storage-change-sets"),
];

/// Formats snapshot compression and upload progress for structured logs.
#[derive(Debug)]
pub struct ProgressDisplay;

impl ProgressDisplay {
    /// Returns the integer completion percentage for a completed and total count.
    pub fn percent(done: u64, total: u64) -> u64 {
        done.saturating_mul(100).checked_div(total).unwrap_or(100)
    }

    /// Returns completion percentage with two decimal places.
    pub fn precise_percent(done: u64, total: u64) -> String {
        if total == 0 {
            return "100.00%".to_string();
        }
        format!("{:.2}%", done as f64 * 100.0 / total as f64)
    }

    /// Formats a byte count with two decimal places and human-readable binary units.
    pub fn bytes(bytes: f64) -> String {
        const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

        let bytes = bytes.max(0.0);
        let unit = if bytes == 0.0 {
            0
        } else {
            ((bytes.log2() / 10.0).floor() as usize).min(UNITS.len() - 1)
        };
        format!("{:.2} {}", bytes / 1024_f64.powi(unit as i32), UNITS[unit])
    }

    /// Formats completed and total byte counts with human-readable binary units.
    pub fn human_byte_progress(done: u64, total: u64) -> String {
        format!("{}/{}", Self::bytes(done as f64), Self::bytes(total as f64))
    }

    /// Formats bytes per second with human-readable binary units.
    pub fn speed(bytes_per_second: f64) -> String {
        format!("{}/s", Self::bytes(bytes_per_second))
    }

    /// Calculates the ETA from average throughput since an operation started.
    pub fn eta(done: u64, total: u64, elapsed: Duration) -> Option<FormattedDuration> {
        if done == 0 || done >= total {
            return None;
        }
        let seconds = total.saturating_sub(done) as f64 / (done as f64 / elapsed.as_secs_f64());
        Some(Self::duration(Duration::from_secs_f64(seconds)))
    }

    /// Formats a duration at whole-second precision for periodic progress logs.
    pub fn duration(duration: Duration) -> FormattedDuration {
        format_duration(Duration::from_secs(duration.as_secs()))
    }
}

/// Convenience helpers for snapshotter-specific manifest lookups.
pub trait SnapshotManifestExt {
    /// Returns the per-file BLAKE3 hashes for a static-file chunk archive,
    /// sorted by file path, or `None` if the chunk has no recorded hashes.
    ///
    /// Returns `None` when:
    /// - `filename` does not match the `{component}-{start}-{end}.tar.zst` pattern,
    /// - the component is not present in the manifest or is not a chunked component,
    /// - the chunk index is out of range, or
    /// - the chunk's hash list is empty (which happens when no prior metadata was
    ///   available for a chunk skipped during generation).
    ///
    /// Callers should treat `None` as "no comparable hash available" and fall
    /// through to re-upload.
    fn chunk_hashes_for_file(&self, filename: &str) -> Option<Vec<String>>;

    /// Returns the full per-file metadata for a static-file chunk archive.
    fn chunk_output_files_for_file(&self, filename: &str) -> Option<Vec<OutputFileChecksum>>;

    /// Returns the compressed size recorded for a static-file chunk archive.
    fn chunk_size_for_file(&self, filename: &str) -> Option<u64>;

    /// Returns whether `filename` is the latest chunk for its component.
    fn is_latest_chunk_file(&self, filename: &str) -> bool;
}

impl SnapshotManifestExt for SnapshotManifest {
    fn chunk_hashes_for_file(&self, filename: &str) -> Option<Vec<String>> {
        let (component, start, _end) = ChunkFilename::parse(filename)?;
        let ComponentManifest::Chunked(meta) = self.components.get(&component)? else {
            return None;
        };
        let chunk_index = usize::try_from(start / meta.blocks_per_file).ok()?;
        let entries = meta.chunk_output_files.get(chunk_index)?;
        if entries.is_empty() {
            return None;
        }
        let mut sorted = entries.clone();
        sorted.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Some(sorted.into_iter().map(|e| e.blake3).collect())
    }

    fn chunk_output_files_for_file(&self, filename: &str) -> Option<Vec<OutputFileChecksum>> {
        let (component, start, _end) = ChunkFilename::parse(filename)?;
        let ComponentManifest::Chunked(meta) = self.components.get(&component)? else {
            return None;
        };
        let chunk_index = usize::try_from(start / meta.blocks_per_file).ok()?;
        let entries = meta.chunk_output_files.get(chunk_index)?;
        if entries.is_empty() {
            return None;
        }
        Some(entries.clone())
    }

    fn chunk_size_for_file(&self, filename: &str) -> Option<u64> {
        let (component, start, _end) = ChunkFilename::parse(filename)?;
        let ComponentManifest::Chunked(meta) = self.components.get(&component)? else {
            return None;
        };
        let chunk_index = usize::try_from(start / meta.blocks_per_file).ok()?;
        meta.chunk_sizes.get(chunk_index).copied()
    }

    fn is_latest_chunk_file(&self, filename: &str) -> bool {
        let Some((component, start, end)) = ChunkFilename::parse(filename) else {
            return false;
        };
        let Some(ComponentManifest::Chunked(meta)) = self.components.get(&component) else {
            return false;
        };
        let Some((latest_start, latest_end)) = latest_chunk_range(meta) else {
            return false;
        };

        start == latest_start && end == latest_end
    }
}

/// Inputs for [`SnapshotGenerator::generate_manifest`].
#[derive(Debug, Clone, Copy)]
pub struct ManifestGenerationParams<'a> {
    /// Reth node datadir containing static files, state DB, and optional proofs DB.
    pub source_datadir: &'a Path,
    /// Directory where snapshot archives and `manifest.json` are written.
    pub output_dir: &'a Path,
    /// Chain ID recorded in the manifest.
    pub chain_id: u64,
    /// Optional base URL recorded in the manifest.
    pub base_url: Option<&'a str>,
    /// Snapshot block height. Inferred from header static files when `None`.
    pub block: Option<u64>,
    /// Blocks per static-file chunk archive. Defaults to `500_000` when `None`.
    pub blocks_per_file: Option<u64>,
    /// Remote static-file chunk filenames and sizes used to verify reusable archives.
    pub remote_static_files: &'a HashMap<String, u64>,
    /// Previously published manifest used to verify reusable archives.
    pub previous_manifest: Option<&'a SnapshotManifest>,
    /// Whether to package `{source_datadir}/proofs` into `proofs.tar.zst`.
    pub upload_proofs: bool,
}

/// Generates snapshot archives with selective compression.
///
/// Static-file chunks are not compressed or written locally when their current
/// uncompressed file metadata matches a previous manifest and the corresponding
/// remote archive has the expected compressed size.
#[derive(Debug)]
pub struct SnapshotGenerator;

impl SnapshotGenerator {
    /// Generates snapshot archives, skipping compression for chunks that already
    /// exist remotely and reusing prior per-file chunk metadata when available.
    ///
    /// Returns the list of files created in the output directory.
    ///
    /// From <https://github.com/paradigmxyz/reth/blob/420693521fccd1437071a15a4a54a3a98b5492cf/crates/cli/commands/src/download/manifest.rs>
    pub fn generate_manifest(params: &ManifestGenerationParams<'_>) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(params.output_dir).with_context(|| {
            format!("failed to create output dir {}", params.output_dir.display())
        })?;

        let blocks_per_file = params.blocks_per_file.unwrap_or(DEFAULT_BLOCKS_PER_FILE);
        let block = match params.block {
            Some(block) => block,
            None => infer_block_from_headers(params.source_datadir)?,
        };
        let total_blocks = block.checked_add(1).context("snapshot block height exceeds u64")?;

        info!(
            source = %params.source_datadir.display(),
            output = %params.output_dir.display(),
            chain_id = params.chain_id,
            block,
            blocks_per_file,
            "generating snapshot archives"
        );

        let static_files_dir = params.source_datadir.join("static_files");
        let static_dir = if static_files_dir.exists() {
            static_files_dir
        } else {
            params.source_datadir.to_path_buf()
        };
        let dir_listing = read_static_dir(&static_dir)?;

        let mut components = BTreeMap::new();

        let num_chunks = total_blocks.div_ceil(blocks_per_file);
        if num_chunks > MAX_CHUNKS {
            bail!(
                "too many chunks ({num_chunks}) for block {block} with blocks_per_file \
                 {blocks_per_file} — increase --blocks-per-file or check --block"
            );
        }

        for &(key, segment_name) in CHUNKED_COMPONENTS {
            let mut planned = Vec::new();
            let mut reuse_candidates = Vec::new();
            let mut found_any = false;
            let mut chunk_sizes = vec![0u64; num_chunks as usize];
            let mut chunk_decompressed = vec![0u64; num_chunks as usize];
            let mut chunk_output_files: Vec<Vec<OutputFileChecksum>> =
                (0..num_chunks).map(|_| Vec::new()).collect();

            for i in 0..num_chunks {
                let start = i * blocks_per_file;
                let end = start.checked_add(blocks_per_file - 1).context("block range overflow")?;
                let source_files = filter_source_files(&dir_listing, segment_name, start, end);

                if source_files.is_empty() {
                    if found_any {
                        bail!("missing source files for {key} chunk {start}-{end}");
                    }
                    continue;
                }
                found_any = true;

                let archive_name = ChunkFilename::format(key, start, end);
                if let (Some(&remote_size), Some(previous_manifest)) =
                    (params.remote_static_files.get(&archive_name), params.previous_manifest)
                {
                    let previous_size = previous_manifest.chunk_size_for_file(&archive_name);
                    if previous_size == Some(remote_size)
                        && let Some(previous_output_files) =
                            previous_manifest.chunk_output_files_for_file(&archive_name)
                    {
                        reuse_candidates.push(ReuseCandidate {
                            chunk: PlannedChunk {
                                chunk_idx: i,
                                archive_path: params.output_dir.join(&archive_name),
                                source_files,
                            },
                            archive_name,
                            remote_size,
                            previous_output_files,
                        });
                        continue;
                    }
                }

                planned.push(PlannedChunk {
                    chunk_idx: i,
                    archive_path: params.output_dir.join(archive_name),
                    source_files,
                });
            }

            if !found_any {
                info!(component = key, "no static files found, skipping component");
            } else {
                let checked_candidates = reuse_candidates
                    .into_par_iter()
                    .map(|candidate| {
                        let output_files =
                            chunk_output_files_for_source_files(&candidate.chunk.source_files)?;
                        Ok((candidate, output_files))
                    })
                    .collect::<Result<Vec<_>>>()?;
                for (candidate, output_files) in checked_candidates {
                    if output_files == candidate.previous_output_files {
                        let idx = candidate.chunk.chunk_idx as usize;
                        chunk_sizes[idx] = candidate.remote_size;
                        chunk_decompressed[idx] = output_files.iter().map(|file| file.size).sum();
                        chunk_output_files[idx] = output_files;
                        info!(archive = %candidate.archive_name, "reusing existing snapshot archive");
                    } else {
                        planned.push(candidate.chunk);
                    }
                }

                let packaged: Vec<PackagedChunk> = planned
                    .into_par_iter()
                    .map(|p| {
                        let output_files = write_chunk_archive(&p.archive_path, &p.source_files)?;
                        let size = std::fs::metadata(&p.archive_path)?.len();
                        Ok(PackagedChunk { chunk_idx: p.chunk_idx, size, output_files })
                    })
                    .collect::<Result<Vec<_>>>()?;

                for p in packaged {
                    let idx = p.chunk_idx as usize;
                    chunk_sizes[idx] = p.size;
                    chunk_decompressed[idx] = p.output_files.iter().map(|f| f.size).sum();
                    chunk_output_files[idx] = p.output_files;
                }

                let total_size: u64 = chunk_sizes.iter().sum();
                info!(
                    component = key,
                    compressed_size = total_size,
                    total_blocks,
                    "packaged chunked component"
                );

                components.insert(
                    key.to_string(),
                    ComponentManifest::Chunked(ChunkedArchive {
                        blocks_per_file,
                        total_blocks,
                        chunk_sizes,
                        chunk_decompressed_sizes: chunk_decompressed,
                        chunk_output_files,
                        chunk_files: vec![],
                    }),
                );
            }
        }

        let mut single_components =
            vec![("state", "state.tar.zst", state_source_files(params.source_datadir)?)];
        let rocksdb_files = rocksdb_source_files(params.source_datadir)?;
        if !rocksdb_files.is_empty() {
            single_components.push(("rocksdb_indices", "rocksdb_indices.tar.zst", rocksdb_files));
        }

        let proofs_files = if params.upload_proofs {
            proofs_source_files(params.source_datadir)?
        } else {
            Vec::new()
        };
        if !proofs_files.is_empty() {
            single_components.push(("proofs", "proofs.tar.zst", proofs_files));
        }

        // These source trees and output archives are independent. Package them on the shared
        // Rayon pool so the snapshotter's `--snapshot-threads` limit applies to this work too.
        let packaged_single_components = single_components
            .into_par_iter()
            .map(|(component, archive_name, files)| {
                let (size, output_files) =
                    package_single_component(params.output_dir, component, archive_name, &files)?;
                let decompressed_size = output_files.iter().map(|file| file.size).sum();
                info!(
                    component = %component,
                    compressed_size = %ProgressDisplay::bytes(size as f64),
                    raw_size = %ProgressDisplay::bytes(decompressed_size as f64),
                    file_count = files.len(),
                    "packaged database component"
                );
                Ok((component, archive_name, size, decompressed_size, output_files))
            })
            .collect::<Result<Vec<_>>>()?;

        for (component, archive_name, size, decompressed_size, output_files) in
            packaged_single_components
        {
            components.insert(
                component.to_string(),
                ComponentManifest::Single(SingleArchive {
                    file: archive_name.to_string(),
                    size,
                    decompressed_size,
                    blake3: None,
                    output_files,
                }),
            );
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_secs();

        let manifest = SnapshotManifest {
            block,
            chain_id: params.chain_id,
            storage_version: 2,
            timestamp,
            base_url: params.base_url.map(str::to_owned),
            reth_version: Some(
                reth_node_core::version::version_metadata().short_version.to_string(),
            ),
            components,
        };

        let manifest_path = params.output_dir.join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        info!(block, components = manifest.components.len(), "manifest written");

        let files = Self::collect_output_files(params.output_dir)?;
        info!(file_count = files.len(), "snapshot generation complete");
        Ok(files)
    }

    /// Collects all files in a snapshot output directory (non-recursive).
    pub fn collect_output_files(dir: &Path) -> Result<Vec<PathBuf>> {
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
}

/// Parser/formatter for chunked static-file archive names of the form
/// `{component}-{start}-{end}.tar.zst` (e.g. `headers-0-499999.tar.zst`).
#[derive(Debug)]
pub struct ChunkFilename;

impl ChunkFilename {
    /// Formats a chunk archive filename.
    pub fn format(component: &str, start: u64, end: u64) -> String {
        format!("{component}-{start}-{end}.tar.zst")
    }

    /// Parses a chunk archive filename into `(component, start, end)`.
    /// Returns `None` if the filename does not match the expected pattern.
    pub fn parse(filename: &str) -> Option<(String, u64, u64)> {
        let stem = filename.strip_suffix(".tar.zst")?;
        let parts: Vec<&str> = stem.rsplitn(3, '-').collect();
        if parts.len() < 3 {
            return None;
        }
        let end = parts[0].parse::<u64>().ok()?;
        let start = parts[1].parse::<u64>().ok()?;
        Some((parts[2].to_string(), start, end))
    }
}

/// Returns the block range for a chunked component's latest archive.
fn latest_chunk_range(meta: &ChunkedArchive) -> Option<(u64, u64)> {
    let latest_start = meta
        .total_blocks
        .checked_sub(1)?
        .checked_div(meta.blocks_per_file)?
        .checked_mul(meta.blocks_per_file)?;
    let latest_end = latest_start.checked_add(meta.blocks_per_file - 1)?;
    Some((latest_start, latest_end))
}

/// Infers the snapshot block from the highest header static file range.
fn infer_block_from_headers(source_datadir: &Path) -> Result<u64> {
    let static_files_dir = source_datadir.join("static_files");
    let dir =
        if static_files_dir.exists() { static_files_dir } else { source_datadir.to_path_buf() };

    let mut max_end = None;
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(range) = parse_headers_range(&name) {
            max_end = Some(max_end.map_or(range.1, |prev: u64| prev.max(range.1)));
        }
    }

    max_end.ok_or_else(|| anyhow::anyhow!("no header static files found to infer --block"))
}

fn parse_headers_range(file_name: &str) -> Option<(u64, u64)> {
    let remainder = file_name.strip_prefix("static_file_headers_")?;
    let (start, end_with_suffix) = remainder.split_once('_')?;
    let start = start.parse::<u64>().ok()?;
    let end_digits: String = end_with_suffix.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    let end = end_digits.parse::<u64>().ok()?;
    Some((start, end))
}

struct PlannedChunk {
    chunk_idx: u64,
    archive_path: PathBuf,
    source_files: Vec<PathBuf>,
}

struct ReuseCandidate {
    chunk: PlannedChunk,
    archive_name: String,
    remote_size: u64,
    previous_output_files: Vec<OutputFileChecksum>,
}

struct PackagedChunk {
    chunk_idx: u64,
    size: u64,
    output_files: Vec<OutputFileChecksum>,
}

struct PlannedFile {
    source_path: PathBuf,
    relative_path: PathBuf,
}

/// Cached directory entry: (filename, full path).
type DirEntry = (String, PathBuf);

/// Reads a directory once, returning all file entries as (name, path) pairs.
fn read_static_dir(dir: &Path) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        entries.push((name, entry.path()));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Filters the cached directory listing for files matching a chunk prefix.
fn filter_source_files(
    dir_listing: &[DirEntry],
    segment_name: &str,
    start: u64,
    end: u64,
) -> Vec<PathBuf> {
    let prefix = format!("static_file_{segment_name}_{start}_{end}");
    dir_listing
        .iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .map(|(_, path)| path.clone())
        .collect()
}

fn state_source_files(source_datadir: &Path) -> Result<Vec<PlannedFile>> {
    let db_dir = source_datadir.join("db");
    if db_dir.exists() {
        return collect_files_recursive(&db_dir, Path::new("db"));
    }

    if looks_like_db_dir(source_datadir)? {
        return collect_files_recursive(source_datadir, Path::new("db"));
    }

    bail!("could not find source state DB directory under {}", source_datadir.display())
}

fn rocksdb_source_files(source_datadir: &Path) -> Result<Vec<PlannedFile>> {
    let rocksdb_dir = source_datadir.join("rocksdb");
    if !rocksdb_dir.exists() {
        return Ok(Vec::new());
    }
    collect_files_recursive(&rocksdb_dir, Path::new("rocksdb"))
}

fn proofs_source_files(source_datadir: &Path) -> Result<Vec<PlannedFile>> {
    let proofs_dir = source_datadir.join("proofs");
    if !proofs_dir.exists() {
        return Ok(Vec::new());
    }
    collect_files_recursive(&proofs_dir, Path::new("proofs"))
}

fn looks_like_db_dir(path: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "mdbx.dat" || name == "lock.mdb" || name == "data.mdb" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_files_recursive(root: &Path, output_prefix: &Path) -> Result<Vec<PlannedFile>> {
    let mut files = Vec::new();
    collect_files_inner(root, root, output_prefix, &mut files)?;
    files.sort_unstable_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

fn collect_files_inner(
    root: &Path,
    dir: &Path,
    output_prefix: &Path,
    files: &mut Vec<PlannedFile>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_files_inner(root, &path, output_prefix, files)?;
        } else if ft.is_file() {
            let relative = path.strip_prefix(root)?.to_path_buf();
            files.push(PlannedFile {
                source_path: path,
                relative_path: output_prefix.join(relative),
            });
        }
    }
    Ok(())
}

fn package_single_component(
    output_dir: &Path,
    component: &str,
    archive_name: &str,
    files: &[PlannedFile],
) -> Result<(u64, Vec<OutputFileChecksum>)> {
    if files.is_empty() {
        bail!("cannot package empty archive: {archive_name}");
    }
    let archive_path = output_dir.join(archive_name);
    let raw_size = files.iter().try_fold(0u64, |total, file| {
        std::fs::metadata(&file.source_path).map(|metadata| total.saturating_add(metadata.len()))
    })?;
    info!(
        component = %component,
        raw_size = %ProgressDisplay::bytes(raw_size as f64),
        file_count = files.len(),
        "packaging database component"
    );
    let mut progress = CompressionProgress::new(component, &archive_path, raw_size);
    let output_files = write_archive_from_planned_files(&archive_path, files, Some(&mut progress))?;
    let size = std::fs::metadata(&archive_path)?.len();
    Ok((size, output_files))
}

fn write_chunk_archive(path: &Path, source_files: &[PathBuf]) -> Result<Vec<OutputFileChecksum>> {
    let planned: Vec<PlannedFile> = source_files
        .iter()
        .map(|p| {
            let file_name =
                p.file_name().ok_or_else(|| anyhow::anyhow!("invalid path: {}", p.display()))?;
            Ok(PlannedFile {
                source_path: p.clone(),
                relative_path: PathBuf::from("static_files").join(file_name),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    write_archive_from_planned_files(path, &planned, None)
}

fn chunk_output_files_for_source_files(
    source_files: &[PathBuf],
) -> Result<Vec<OutputFileChecksum>> {
    let planned: Vec<PlannedFile> = source_files
        .iter()
        .map(|p| {
            let file_name =
                p.file_name().ok_or_else(|| anyhow::anyhow!("invalid path: {}", p.display()))?;
            Ok(PlannedFile {
                source_path: p.clone(),
                relative_path: PathBuf::from("static_files").join(file_name),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    compute_output_files_for_planned_files(&planned)
}

fn write_archive_from_planned_files(
    path: &Path,
    files: &[PlannedFile],
    progress: Option<&mut CompressionProgress>,
) -> Result<Vec<OutputFileChecksum>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = zstd::Encoder::new(file, 0)?;
    encoder.include_checksum(true)?;
    let mut builder = tar::Builder::new(encoder);

    let output_files =
        compute_output_files_and_archive(files, Some((&mut builder, path)), progress)?;

    let encoder = builder.into_inner()?;
    encoder.finish()?;

    Ok(output_files)
}

fn compute_output_files_for_planned_files(
    files: &[PlannedFile],
) -> Result<Vec<OutputFileChecksum>> {
    compute_output_files_and_archive(files, None, None)
}

fn compute_output_files_and_archive(
    files: &[PlannedFile],
    mut archive: Option<(&mut tar::Builder<zstd::Encoder<'_, std::fs::File>>, &Path)>,
    mut progress: Option<&mut CompressionProgress>,
) -> Result<Vec<OutputFileChecksum>> {
    let mut output_files = Vec::with_capacity(files.len());
    for planned in files {
        let expected_size = std::fs::metadata(&planned.source_path)?.len();

        let source_file = std::fs::File::open(&planned.source_path)?;
        let mut reader = HashingReader::new(source_file, progress.as_deref_mut());

        if let Some((builder, archive_path)) = archive.as_mut() {
            let mut header = tar::Header::new_gnu();
            header.set_size(expected_size);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, &planned.relative_path, &mut reader).with_context(
                || {
                    format!(
                        "failed to append {} to {}",
                        planned.source_path.display(),
                        archive_path.display()
                    )
                },
            )?;
        } else {
            std::io::copy(&mut reader, &mut std::io::sink())?;
        }

        if reader.bytes_read != expected_size {
            bail!(
                "file size changed during archiving: {} (expected {expected_size}, read {})",
                planned.source_path.display(),
                reader.bytes_read
            );
        }

        output_files.push(OutputFileChecksum {
            path: planned.relative_path.to_string_lossy().to_string(),
            size: reader.bytes_read,
            blake3: reader.finalize(),
        });
    }

    Ok(output_files)
}

struct CompressionProgress {
    component: String,
    archive_path: PathBuf,
    raw_size: u64,
    raw_processed: u64,
    started: Instant,
    last_log: Instant,
}

impl CompressionProgress {
    fn new(component: &str, archive_path: &Path, raw_size: u64) -> Self {
        let now = Instant::now();
        Self {
            component: component.to_string(),
            archive_path: archive_path.to_path_buf(),
            raw_size,
            raw_processed: 0,
            started: now,
            last_log: now,
        }
    }

    fn add(&mut self, bytes: u64) {
        self.raw_processed = self.raw_processed.saturating_add(bytes).min(self.raw_size);
        if self.last_log.elapsed() < COMPRESSION_LOG_INTERVAL {
            return;
        }

        let elapsed = self.started.elapsed();
        let compressed_size =
            std::fs::metadata(&self.archive_path).map_or(0, |metadata| metadata.len());
        let speed = self.raw_processed as f64 / elapsed.as_secs_f64();
        let eta = ProgressDisplay::eta(self.raw_processed, self.raw_size, elapsed)
            .map_or_else(|| "unknown".to_string(), |eta| eta.to_string());
        info!(
            component = %self.component,
            raw_size = %ProgressDisplay::bytes(self.raw_size as f64),
            raw_processed = %ProgressDisplay::bytes(self.raw_processed as f64),
            compressed_size = %ProgressDisplay::bytes(compressed_size as f64),
            progress = %ProgressDisplay::precise_percent(self.raw_processed, self.raw_size),
            speed = %ProgressDisplay::speed(speed),
            eta = %eta,
            elapsed = %ProgressDisplay::duration(elapsed),
            "packaging database component (progress)"
        );
        self.last_log = Instant::now();
    }
}

struct HashingReader<'a, R> {
    inner: R,
    hasher: blake3::Hasher,
    bytes_read: u64,
    progress: Option<&'a mut CompressionProgress>,
}

impl<'a, R: Read> HashingReader<'a, R> {
    fn new(inner: R, progress: Option<&'a mut CompressionProgress>) -> Self {
        Self { inner, hasher: blake3::Hasher::new(), bytes_read: 0, progress }
    }

    fn finalize(self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl<R: Read> Read for HashingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.bytes_read += n as u64;
            self.hasher.update(&buf[..n]);
            if let Some(progress) = self.progress.as_mut() {
                progress.add(n as u64);
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest_params<'a>(
        source_datadir: &'a Path,
        output_dir: &'a Path,
        remote_static_files: &'a HashMap<String, u64>,
        previous_manifest: Option<&'a SnapshotManifest>,
        block: Option<u64>,
        upload_proofs: bool,
    ) -> ManifestGenerationParams<'a> {
        ManifestGenerationParams {
            source_datadir,
            output_dir,
            chain_id: 8453,
            base_url: None,
            block,
            blocks_per_file: Some(500_000),
            remote_static_files,
            previous_manifest,
            upload_proofs,
        }
    }

    fn previous_headers_manifest(
        output_files: Vec<OutputFileChecksum>,
        size: u64,
    ) -> SnapshotManifest {
        SnapshotManifest {
            block: 2_000_000,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 0,
            base_url: None,
            reth_version: None,
            components: BTreeMap::from([(
                "headers".to_string(),
                ComponentManifest::Chunked(ChunkedArchive {
                    blocks_per_file: 500_000,
                    total_blocks: 2_000_000,
                    chunk_sizes: vec![size, 0, 0, 0],
                    chunk_decompressed_sizes: vec![],
                    chunk_output_files: vec![output_files, vec![], vec![], vec![]],
                    chunk_files: vec![],
                }),
            )]),
        }
    }

    #[test]
    fn parse_headers_range_valid() {
        assert_eq!(parse_headers_range("static_file_headers_0_499999"), Some((0, 499_999)));
        assert_eq!(
            parse_headers_range("static_file_headers_500000_999999"),
            Some((500_000, 999_999))
        );
    }

    #[test]
    fn parse_headers_range_with_suffix() {
        assert_eq!(
            parse_headers_range("static_file_headers_500000_999999.jar"),
            Some((500_000, 999_999))
        );
    }

    #[test]
    fn parse_headers_range_non_header_files() {
        assert_eq!(parse_headers_range("static_file_transactions_0_499999"), None);
        assert_eq!(parse_headers_range("mdbx.dat"), None);
        assert_eq!(parse_headers_range(""), None);
    }

    #[test]
    fn infer_block_from_headers_uses_max_end() {
        let dir = tempfile::tempdir().unwrap();
        let sf = dir.path().join("static_files");
        std::fs::create_dir_all(&sf).unwrap();
        std::fs::write(sf.join("static_file_headers_0_499999"), []).unwrap();
        std::fs::write(sf.join("static_file_headers_500000_999999"), []).unwrap();

        assert_eq!(infer_block_from_headers(dir.path()).unwrap(), 999_999);
    }

    #[test]
    fn infer_block_from_headers_fails_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("static_files")).unwrap();

        assert!(infer_block_from_headers(dir.path()).is_err());
    }

    #[test]
    fn is_latest_chunk_file_identifies_only_the_final_chunk() {
        let mut components = BTreeMap::new();
        components.insert(
            "headers".to_string(),
            ComponentManifest::Chunked(ChunkedArchive {
                blocks_per_file: 500_000,
                total_blocks: 1_000_000,
                chunk_sizes: vec![1, 1],
                chunk_decompressed_sizes: vec![1, 1],
                chunk_output_files: vec![vec![], vec![]],
                chunk_files: vec![],
            }),
        );
        let manifest = SnapshotManifest {
            block: 1_000_000,
            chain_id: 8453,
            storage_version: 2,
            timestamp: 0,
            base_url: None,
            reth_version: None,
            components,
        };

        assert!(
            manifest.is_latest_chunk_file("headers-500000-999999.tar.zst"),
            "latest chunk should be identified"
        );
        assert!(
            !manifest.is_latest_chunk_file("headers-0-499999.tar.zst"),
            "finalized chunk should not be identified as latest"
        );
        assert!(
            !manifest.is_latest_chunk_file("headers-500000-999998.tar.zst"),
            "wrong range end should not be identified as latest"
        );
        assert!(
            !manifest.is_latest_chunk_file("state.tar.zst"),
            "single archive should not be identified as a latest chunk"
        );
    }

    #[test]
    fn generate_manifest_creates_state_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state-data").unwrap();

        let remote = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            None,
            Some(0),
            false,
        ))
        .unwrap();

        assert!(
            files.iter().any(|f| f.file_name().unwrap() == "state.tar.zst"),
            "should produce state.tar.zst"
        );
        assert!(
            files.iter().any(|f| f.file_name().unwrap() == "manifest.json"),
            "should produce manifest.json"
        );
    }

    #[test]
    fn generate_manifest_includes_chunk_containing_exact_block_height() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state-data").unwrap();

        let static_files_dir = source.path().join("static_files");
        std::fs::create_dir_all(&static_files_dir).unwrap();
        std::fs::write(static_files_dir.join("static_file_headers_0_499999"), b"first").unwrap();
        std::fs::write(static_files_dir.join("static_file_headers_500000_999999"), b"second")
            .unwrap();

        let remote = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            None,
            Some(500_000),
            false,
        ))
        .unwrap();

        assert!(files.iter().any(|path| {
            path.file_name().is_some_and(|name| name == "headers-500000-999999.tar.zst")
        }));

        let manifest: SnapshotManifest =
            serde_json::from_slice(&std::fs::read(output.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.block, 500_000);
        let ComponentManifest::Chunked(headers) = &manifest.components["headers"] else {
            panic!("headers component should be chunked");
        };
        assert_eq!(headers.total_blocks, 500_001);
        assert_eq!(headers.chunk_sizes.len(), 2);
        assert_eq!(headers.chunk_output_files.len(), 2);
        assert!(!headers.chunk_output_files[1].is_empty());
    }

    #[test]
    fn generate_manifest_creates_proofs_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state-data").unwrap();

        let proofs_dir = source.path().join("proofs");
        std::fs::create_dir_all(&proofs_dir).unwrap();
        std::fs::write(proofs_dir.join("CURRENT"), b"MANIFEST-000014\n").unwrap();
        std::fs::write(proofs_dir.join("IDENTITY"), b"identity-bytes").unwrap();
        std::fs::write(proofs_dir.join("LOCK"), b"").unwrap();
        std::fs::write(proofs_dir.join("MANIFEST-000014"), b"manifest-data").unwrap();
        std::fs::write(proofs_dir.join("OPTIONS-000007"), b"options-data").unwrap();
        std::fs::write(proofs_dir.join("000060.sst"), b"sst-data").unwrap();
        std::fs::write(proofs_dir.join("000801.log"), b"wal-data").unwrap();

        let remote = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            None,
            Some(0),
            true,
        ))
        .unwrap();

        assert!(
            files.iter().any(|f| f.file_name().unwrap() == "proofs.tar.zst"),
            "should produce proofs.tar.zst when proofs/ exists"
        );

        let manifest_content =
            std::fs::read_to_string(output.path().join("manifest.json")).unwrap();
        let manifest: SnapshotManifest = serde_json::from_str(&manifest_content).unwrap();
        let ComponentManifest::Single(proofs) =
            manifest.components.get("proofs").expect("manifest should include proofs component")
        else {
            panic!("proofs component should be a Single archive");
        };

        assert_eq!(proofs.file, "proofs.tar.zst", "proofs archive filename");
        assert_eq!(proofs.output_files.len(), 7, "exactly 7 proofs DB files should be packaged");
        assert!(
            proofs.output_files.iter().all(|f| f.path.starts_with("proofs/")),
            "all proofs output paths should be under proofs/"
        );
        assert!(
            proofs.output_files.iter().any(|f| f.path == "proofs/000060.sst"),
            "should include SST file under proofs/"
        );
        assert!(
            proofs.output_files.iter().any(|f| f.path == "proofs/CURRENT"),
            "should include CURRENT under proofs/"
        );
    }

    #[test]
    fn generate_manifest_skips_proofs_when_missing() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state-data").unwrap();

        let remote = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            None,
            Some(0),
            true,
        ))
        .unwrap();

        assert!(
            !files.iter().any(|f| f.file_name().unwrap() == "proofs.tar.zst"),
            "should not produce proofs.tar.zst when proofs/ is missing"
        );

        let manifest_content =
            std::fs::read_to_string(output.path().join("manifest.json")).unwrap();
        let manifest: SnapshotManifest = serde_json::from_str(&manifest_content).unwrap();
        assert!(
            !manifest.components.contains_key("proofs"),
            "manifest should omit proofs component when proofs/ is missing"
        );
    }

    #[test]
    fn generate_manifest_skips_proofs_when_upload_disabled() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state-data").unwrap();

        let proofs_dir = source.path().join("proofs");
        std::fs::create_dir_all(&proofs_dir).unwrap();
        std::fs::write(proofs_dir.join("CURRENT"), b"MANIFEST-000014\n").unwrap();

        let remote = HashMap::new();
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            None,
            Some(0),
            false,
        ))
        .unwrap();

        assert!(
            !files.iter().any(|f| f.file_name().unwrap() == "proofs.tar.zst"),
            "should not produce proofs.tar.zst when upload_proofs is disabled"
        );

        let manifest_content =
            std::fs::read_to_string(output.path().join("manifest.json")).unwrap();
        let manifest: SnapshotManifest = serde_json::from_str(&manifest_content).unwrap();
        assert!(
            !manifest.components.contains_key("proofs"),
            "manifest should omit proofs component when upload_proofs is disabled"
        );
    }

    #[test]
    fn generate_manifest_reuses_archive_when_source_hashes_match() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state").unwrap();

        // Four header chunks, with the first already present remotely.
        let sf = source.path().join("static_files");
        std::fs::create_dir_all(&sf).unwrap();
        for i in 0..4u64 {
            let start = i * 500_000;
            let end = (i + 1) * 500_000 - 1;
            std::fs::write(sf.join(format!("static_file_headers_{start}_{end}")), b"data").unwrap();
        }

        let remote = HashMap::from([("headers-0-499999.tar.zst".to_string(), 123)]);
        let previous_manifest = previous_headers_manifest(
            vec![OutputFileChecksum {
                path: "static_files/static_file_headers_0_499999".to_string(),
                size: 4,
                blake3: blake3::hash(b"data").to_hex().to_string(),
            }],
            123,
        );
        let files = SnapshotGenerator::generate_manifest(&test_manifest_params(
            source.path(),
            output.path(),
            &remote,
            Some(&previous_manifest),
            Some(1_999_999),
            false,
        ))
        .unwrap();

        let filenames: Vec<String> = files
            .iter()
            .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();

        assert!(
            !filenames.contains(&"headers-0-499999.tar.zst".to_string()),
            "matching remote archive should not be recreated"
        );
        assert!(
            filenames.contains(&"headers-500000-999999.tar.zst".to_string()),
            "range without a shared remote archive should be compressed"
        );
        assert!(
            filenames.contains(&"headers-1000000-1499999.tar.zst".to_string()),
            "range without a shared remote archive should be compressed"
        );
        assert!(
            filenames.contains(&"headers-1500000-1999999.tar.zst".to_string()),
            "range without a remote archive should be compressed"
        );
    }

    #[test]
    fn source_hash_mismatch_recreates_archive() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state").unwrap();

        // Four header chunks, with stale metadata for the first archive.
        let sf = source.path().join("static_files");
        std::fs::create_dir_all(&sf).unwrap();
        for i in 0..4u64 {
            let start = i * 500_000;
            let end = (i + 1) * 500_000 - 1;
            std::fs::write(sf.join(format!("static_file_headers_{start}_{end}")), b"data").unwrap();
        }

        let remote = HashMap::from([("headers-0-499999.tar.zst".to_string(), 123)]);

        let previous_manifest = previous_headers_manifest(
            vec![OutputFileChecksum {
                path: "static_files/static_file_headers_0_499999".to_string(),
                size: 4,
                blake3: blake3::hash(b"different").to_hex().to_string(),
            }],
            123,
        );

        let files = SnapshotGenerator::generate_manifest(&ManifestGenerationParams {
            source_datadir: source.path(),
            output_dir: output.path(),
            chain_id: 8453,
            base_url: None,
            block: Some(1_999_999),
            blocks_per_file: Some(500_000),
            remote_static_files: &remote,
            previous_manifest: Some(&previous_manifest),
            upload_proofs: false,
        })
        .unwrap();

        assert!(
            files.iter().any(|path| path
                .file_name()
                .is_some_and(|name| name == "headers-0-499999.tar.zst")),
            "hash mismatch must recreate the archive"
        );

        let manifest_content =
            std::fs::read_to_string(output.path().join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();

        let headers = &manifest["components"]["headers"];
        let chunk_output_files = headers["chunk_output_files"]
            .as_array()
            .expect("chunk_output_files should be an array");

        assert_eq!(chunk_output_files.len(), 4, "should have 4 chunk entries");
        assert!(
            chunk_output_files[0].as_array().is_some_and(|files| !files.is_empty()),
            "generated chunk should include output-file metadata"
        );
        assert_eq!(
            chunk_output_files[0][0]["blake3"],
            blake3::hash(b"data").to_hex().to_string(),
            "manifest must describe the newly generated archive"
        );
        assert!(
            headers.get("chunk_skipped").is_none(),
            "published manifest should not encode upload-time skip decisions"
        );
    }
}
