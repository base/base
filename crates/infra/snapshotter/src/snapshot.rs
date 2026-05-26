//! Snapshot archive generation with selective compression.
//!
//! Archive creation, BLAKE3 hashing, and manifest structure are derived from
//! [reth](https://github.com/paradigmxyz/reth) (`crates/cli/commands/src/download/manifest.rs`,
//! commit `d58c6e3`, tag `v2.1.0`), licensed under Apache-2.0.
//!
//! Modified to support skipping compression of finalized static file chunks
//! that already exist in remote storage. Only the tip chunk and a configurable
//! buffer of recent chunks are compressed.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Default blocks per static file segment.
const DEFAULT_BLOCKS_PER_FILE: u64 = 500_000;

/// Number of extra chunks beyond the tip to compress as a safety buffer.
const EXTRA_CHUNKS_BUFFER: u64 = 2;

/// Maximum number of chunks allowed before bailing to prevent OOM.
/// At 500k blocks per file, 100k chunks covers 50 billion blocks.
const MAX_CHUNKS: u64 = 100_000;

/// Static file component types that produce chunked archives.
const CHUNKED_COMPONENTS: &[(&str, &str)] = &[
    ("headers", "headers"),
    ("transactions", "transactions"),
    ("transaction_senders", "transaction-senders"),
    ("receipts", "receipts"),
    ("account_changesets", "account-change-sets"),
    ("storage_changesets", "storage-change-sets"),
];

/// A snapshot manifest describing available components.
///
/// Matches reth's `SnapshotManifest` JSON format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Block number this snapshot was taken at.
    pub block: u64,
    /// Chain ID.
    pub chain_id: u64,
    /// Storage version.
    pub storage_version: u64,
    /// Unix timestamp.
    pub timestamp: u64,
    /// Available snapshot components.
    pub components: BTreeMap<String, serde_json::Value>,
}

/// Checksum metadata for an extracted file within an archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFileChecksum {
    /// Relative path under the target datadir.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// BLAKE3 checksum.
    pub blake3: String,
}

/// Generates snapshot archives with selective compression.
///
/// Static file chunks whose block ranges are in `skip_ranges` are not
/// compressed or written to the output directory.
#[derive(Debug)]
pub struct SnapshotGenerator;

impl SnapshotGenerator {
    /// Generates snapshot archives, skipping compression for chunks in `skip_ranges`.
    ///
    /// `skip_ranges` contains `(start, end)` block ranges that already exist
    /// remotely and don't need to be re-compressed.
    ///
    /// Returns the list of files created in the output directory.
    ///
    /// From <https://github.com/paradigmxyz/reth/blob/420693521fccd1437071a15a4a54a3a98b5492cf/crates/cli/commands/src/download/manifest.rs>
    pub fn generate_manifest(
        source_datadir: &Path,
        output_dir: &Path,
        chain_id: u64,
        block: Option<u64>,
        blocks_per_file: Option<u64>,
        remote_static_files: &HashMap<String, u64>,
    ) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create output dir {}", output_dir.display()))?;

        let blocks_per_file = blocks_per_file.unwrap_or(DEFAULT_BLOCKS_PER_FILE);
        let block = match block {
            Some(b) => b,
            None => infer_block_from_headers(source_datadir)?,
        };

        let remote_filenames: HashSet<&str> =
            remote_static_files.keys().map(String::as_str).collect();
        let skip_ranges = Self::compute_skip_ranges(&remote_filenames, block, blocks_per_file)?;

        info!(
            source = %source_datadir.display(),
            output = %output_dir.display(),
            chain_id,
            block,
            blocks_per_file,
            skip_count = skip_ranges.len(),
            "generating snapshot archives"
        );

        let static_files_dir = source_datadir.join("static_files");
        let static_dir =
            if static_files_dir.exists() { static_files_dir } else { source_datadir.to_path_buf() };
        let dir_listing = read_static_dir(&static_dir)?;

        let mut components = BTreeMap::new();

        let num_chunks = block.div_ceil(blocks_per_file);
        if num_chunks > MAX_CHUNKS {
            bail!(
                "too many chunks ({num_chunks}) for block {block} with blocks_per_file \
                 {blocks_per_file} — increase --blocks-per-file or check --block"
            );
        }

        for &(key, segment_name) in CHUNKED_COMPONENTS {
            let mut planned = Vec::new();
            let mut found_any = false;
            let mut chunk_skipped = vec![false; num_chunks as usize];

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

                if skip_ranges.contains(&(start, end)) {
                    chunk_skipped[i as usize] = true;
                    continue;
                }

                planned.push(PlannedChunk {
                    chunk_idx: i,
                    archive_path: output_dir.join(chunk_filename(key, start, end)),
                    source_files,
                });
            }

            if !found_any {
                info!(component = key, "no static files found, skipping component");
            } else {
                let packaged: Vec<PackagedChunk> = planned
                    .into_par_iter()
                    .map(|p| {
                        let output_files = write_chunk_archive(&p.archive_path, &p.source_files)?;
                        let size = std::fs::metadata(&p.archive_path)?.len();
                        Ok(PackagedChunk { chunk_idx: p.chunk_idx, size, output_files })
                    })
                    .collect::<Result<Vec<_>>>()?;

                let mut chunk_sizes = vec![0u64; num_chunks as usize];
                let mut chunk_decompressed = vec![0u64; num_chunks as usize];
                let mut chunk_output_files: Vec<Vec<OutputFileChecksum>> =
                    (0..num_chunks).map(|_| Vec::new()).collect();

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
                    total_blocks = block,
                    "packaged chunked component"
                );

                components.insert(
                    key.to_string(),
                    serde_json::json!({
                        "blocks_per_file": blocks_per_file,
                        "total_blocks": block,
                        "chunk_sizes": chunk_sizes,
                        "chunk_decompressed_sizes": chunk_decompressed,
                        "chunk_output_files": chunk_output_files,
                        "chunk_skipped": chunk_skipped,
                    }),
                );
            }
        }

        let state_files = state_source_files(source_datadir)?;
        let (state_size, state_output_files) =
            package_single_component(output_dir, "state.tar.zst", &state_files)?;
        components.insert(
            "state".to_string(),
            serde_json::json!({
                "file": "state.tar.zst",
                "size": state_size,
                "decompressed_size": state_output_files.iter().map(|f| f.size).sum::<u64>(),
                "output_files": state_output_files,
            }),
        );

        let rocksdb_files = rocksdb_source_files(source_datadir)?;
        if !rocksdb_files.is_empty() {
            let (rocksdb_size, rocksdb_output_files) =
                package_single_component(output_dir, "rocksdb_indices.tar.zst", &rocksdb_files)?;
            components.insert(
                "rocksdb_indices".to_string(),
                serde_json::json!({
                    "file": "rocksdb_indices.tar.zst",
                    "size": rocksdb_size,
                    "decompressed_size": rocksdb_output_files.iter().map(|f| f.size).sum::<u64>(),
                    "output_files": rocksdb_output_files,
                }),
            );
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_secs();

        let manifest =
            SnapshotManifest { block, chain_id, storage_version: 2, timestamp, components };

        let manifest_path = output_dir.join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        info!(block, components = manifest.components.len(), "manifest written");

        let files = collect_output_files(output_dir)?;
        info!(file_count = files.len(), "snapshot generation complete");
        Ok(files)
    }

    /// Determines which chunk ranges can be skipped based on what already exists
    /// remotely. Keeps the tip chunk and `EXTRA_CHUNKS_BUFFER` additional chunks.
    pub fn compute_skip_ranges(
        remote_filenames: &HashSet<&str>,
        block: u64,
        blocks_per_file: u64,
    ) -> Result<HashSet<(u64, u64)>> {
        let num_chunks = block.div_ceil(blocks_per_file);
        let keep_from = num_chunks.saturating_sub(1 + EXTRA_CHUNKS_BUFFER);

        let mut skip = HashSet::new();
        for i in 0..num_chunks {
            if i >= keep_from {
                continue;
            }
            let start = i * blocks_per_file;
            let end = start
                .checked_add(blocks_per_file - 1)
                .context("block range overflow in skip computation")?;

            let dominated_by_remote = CHUNKED_COMPONENTS.iter().all(|&(key, _)| {
                let filename = chunk_filename(key, start, end);
                remote_filenames.contains(filename.as_str())
            });

            if dominated_by_remote {
                skip.insert((start, end));
            }
        }

        Ok(skip)
    }
}

fn chunk_filename(component_key: &str, start: u64, end: u64) -> String {
    format!("{component_key}-{start}-{end}.tar.zst")
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
    archive_name: &str,
    files: &[PlannedFile],
) -> Result<(u64, Vec<OutputFileChecksum>)> {
    if files.is_empty() {
        bail!("cannot package empty archive: {archive_name}");
    }
    let archive_path = output_dir.join(archive_name);
    let output_files = write_archive_from_planned_files(&archive_path, files)?;
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

    write_archive_from_planned_files(path, &planned)
}

fn write_archive_from_planned_files(
    path: &Path,
    files: &[PlannedFile],
) -> Result<Vec<OutputFileChecksum>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = zstd::Encoder::new(file, 0)?;
    encoder.include_checksum(true)?;
    let mut builder = tar::Builder::new(encoder);

    let mut output_files = Vec::with_capacity(files.len());
    for planned in files {
        let expected_size = std::fs::metadata(&planned.source_path)?.len();
        let mut header = tar::Header::new_gnu();
        header.set_size(expected_size);
        header.set_mode(0o644);
        header.set_cksum();

        let source_file = std::fs::File::open(&planned.source_path)?;
        let mut reader = HashingReader::new(source_file);
        builder.append_data(&mut header, &planned.relative_path, &mut reader)?;

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

    let encoder = builder.into_inner()?;
    encoder.finish()?;

    Ok(output_files)
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    bytes_read: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, hasher: blake3::Hasher::new(), bytes_read: 0 }
    }

    fn finalize(self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.bytes_read += n as u64;
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn compute_skip_ranges_skips_finalized_chunks() {
        let mut remote = HashSet::new();
        for &(key, _) in CHUNKED_COMPONENTS {
            remote.insert(chunk_filename(key, 0, 499_999));
            remote.insert(chunk_filename(key, 500_000, 999_999));
            remote.insert(chunk_filename(key, 1_000_000, 1_499_999));
            remote.insert(chunk_filename(key, 1_500_000, 1_999_999));
        }

        // block=2_000_000, blocks_per_file=500_000 → 4 chunks (indices 0-3)
        // tip = chunk 3, buffer = 2 → keep chunks 1,2,3 → skip chunk 0
        let refs: HashSet<&str> = remote.iter().map(String::as_str).collect();
        let skip = SnapshotGenerator::compute_skip_ranges(&refs, 2_000_000, 500_000).unwrap();

        assert!(skip.contains(&(0, 499_999)), "chunk 0 should be skipped");
        assert!(!skip.contains(&(500_000, 999_999)), "chunk 1 should NOT be skipped (in buffer)");
        assert!(
            !skip.contains(&(1_000_000, 1_499_999)),
            "chunk 2 should NOT be skipped (in buffer)"
        );
        assert!(!skip.contains(&(1_500_000, 1_999_999)), "chunk 3 (tip) should NOT be skipped");
    }

    #[test]
    fn compute_skip_ranges_keeps_all_when_few_chunks() {
        let mut remote = HashSet::new();
        for &(key, _) in CHUNKED_COMPONENTS {
            remote.insert(chunk_filename(key, 0, 499_999));
            remote.insert(chunk_filename(key, 500_000, 999_999));
        }

        // block=1_000_000 → 2 chunks, tip + buffer(2) = 3 → keep all
        let refs: HashSet<&str> = remote.iter().map(String::as_str).collect();
        let skip = SnapshotGenerator::compute_skip_ranges(&refs, 1_000_000, 500_000).unwrap();
        assert!(skip.is_empty(), "should keep all chunks when count <= tip + buffer");
    }

    #[test]
    fn compute_skip_ranges_skips_nothing_when_remote_empty() {
        let remote = HashSet::new();
        let refs: HashSet<&str> = remote.iter().map(String::as_str).collect();
        let skip = SnapshotGenerator::compute_skip_ranges(&refs, 5_000_000, 500_000).unwrap();
        assert!(skip.is_empty(), "nothing to skip when remote is empty");
    }

    #[test]
    fn compute_skip_ranges_requires_all_components_present() {
        let mut remote = HashSet::new();
        // Only add headers, not other components
        remote.insert(chunk_filename("headers", 0, 499_999));

        let refs: HashSet<&str> = remote.iter().map(String::as_str).collect();
        let skip = SnapshotGenerator::compute_skip_ranges(&refs, 5_000_000, 500_000).unwrap();
        assert!(
            !skip.contains(&(0, 499_999)),
            "should not skip range if not all components are present remotely"
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
        let files = SnapshotGenerator::generate_manifest(
            source.path(),
            output.path(),
            8453,
            Some(0),
            Some(500_000),
            &remote,
        )
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
    fn generate_manifest_skips_finalized_ranges_via_remote() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state").unwrap();

        // 4 header chunks → block=2M, bpf=500k
        // tip=chunk3, buffer=2 → keep 1,2,3 → skip chunk 0
        let sf = source.path().join("static_files");
        std::fs::create_dir_all(&sf).unwrap();
        for i in 0..4u64 {
            let start = i * 500_000;
            let end = (i + 1) * 500_000 - 1;
            std::fs::write(sf.join(format!("static_file_headers_{start}_{end}")), b"data").unwrap();
        }

        // Simulate all chunked components existing remotely for range 0-499999
        let mut remote = HashMap::new();
        for &(key, _) in CHUNKED_COMPONENTS {
            remote.insert(chunk_filename(key, 0, 499_999), 0u64);
        }

        let files = SnapshotGenerator::generate_manifest(
            source.path(),
            output.path(),
            8453,
            Some(2_000_000),
            Some(500_000),
            &remote,
        )
        .unwrap();

        let filenames: Vec<String> = files
            .iter()
            .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();

        assert!(
            !filenames.contains(&"headers-0-499999.tar.zst".to_string()),
            "finalized range 0 should be skipped (all components exist remotely)"
        );
        assert!(
            filenames.contains(&"headers-500000-999999.tar.zst".to_string()),
            "buffer range should be compressed"
        );
        assert!(
            filenames.contains(&"headers-1500000-1999999.tar.zst".to_string()),
            "tip range should be compressed"
        );
    }

    #[test]
    fn manifest_includes_chunk_skipped_field() {
        let source = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();

        let db_dir = source.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::write(db_dir.join("mdbx.dat"), b"state").unwrap();

        // 4 header chunks → skip chunk 0
        let sf = source.path().join("static_files");
        std::fs::create_dir_all(&sf).unwrap();
        for i in 0..4u64 {
            let start = i * 500_000;
            let end = (i + 1) * 500_000 - 1;
            std::fs::write(sf.join(format!("static_file_headers_{start}_{end}")), b"data").unwrap();
        }

        let mut remote = HashMap::new();
        for &(key, _) in CHUNKED_COMPONENTS {
            remote.insert(chunk_filename(key, 0, 499_999), 0u64);
        }

        SnapshotGenerator::generate_manifest(
            source.path(),
            output.path(),
            8453,
            Some(2_000_000),
            Some(500_000),
            &remote,
        )
        .unwrap();

        let manifest_content =
            std::fs::read_to_string(output.path().join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();

        let headers = &manifest["components"]["headers"];
        let skipped =
            headers["chunk_skipped"].as_array().expect("chunk_skipped should be an array");

        assert_eq!(skipped.len(), 4, "should have 4 chunk entries");
        assert_eq!(skipped[0], true, "chunk 0 should be marked as skipped");
        assert_eq!(skipped[1], false, "chunk 1 (buffer) should not be skipped");
        assert_eq!(skipped[2], false, "chunk 2 (buffer) should not be skipped");
        assert_eq!(skipped[3], false, "chunk 3 (tip) should not be skipped");
    }
}
