//! Wire types for modular snapshot manifests, compatible with
//! `reth_cli_commands::download::manifest`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A snapshot manifest describes available components for a snapshot at a given block height.
///
/// Each component is either a single archive (state) or a set of chunked archives (static file
/// segments like transactions, receipts, etc). Chunked components use `blocks_per_file` to
/// define the block range per archive, matching reth's static file segment boundaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Block number this snapshot was taken at.
    pub block: u64,
    /// Chain ID.
    pub chain_id: u64,
    /// Storage version (1 = legacy, 2 = current).
    pub storage_version: u64,
    /// Timestamp when the snapshot was created (unix seconds).
    pub timestamp: u64,
    /// Base URL for archive downloads. Component archive URLs are relative to this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Reth version that produced this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reth_version: Option<String>,
    /// Available snapshot components.
    pub components: BTreeMap<String, ComponentManifest>,
    /// Chain-specific manifest fields not interpreted by Reth.
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Manifest entry for a single snapshot component.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ComponentManifest {
    /// A single archive file (used for state).
    Single(SingleArchive),
    /// A set of chunked archives split by block range (used for static file segments).
    Chunked(ChunkedArchive),
}

/// A single, non-chunked archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleArchive {
    /// Archive file name (relative to `base_url`).
    pub file: String,
    /// Compressed archive size in bytes.
    pub size: u64,
    /// Total extracted plain-output size in bytes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub decompressed_size: u64,
    /// Optional BLAKE3 checksum of the compressed archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    /// Expected extracted plain files for this archive.
    #[serde(default)]
    pub output_files: Vec<OutputFileChecksum>,
}

/// A chunked archive set where each chunk covers a fixed block range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkedArchive {
    /// Number of blocks per archive file. Matches reth's `blocks_per_file` config.
    pub blocks_per_file: u64,
    /// Total number of blocks covered by this component.
    pub total_blocks: u64,
    /// Compressed size of each chunk in bytes, ordered from first to last.
    #[serde(default)]
    pub chunk_sizes: Vec<u64>,
    /// Extracted plain-output size of each chunk in bytes, ordered from first to last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunk_decompressed_sizes: Vec<u64>,
    /// Archive path for each chunk, relative to [`SnapshotManifest::base_url`], ordered from
    /// first to last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunk_files: Vec<String>,
    /// Expected extracted plain files per chunk, ordered from first to last.
    #[serde(default)]
    pub chunk_output_files: Vec<Vec<OutputFileChecksum>>,
}

impl ChunkedArchive {
    /// Returns the number of chunks.
    pub const fn num_chunks(&self) -> u64 {
        self.total_blocks.div_ceil(self.blocks_per_file)
    }
}

/// Expected metadata for one extracted plain file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFileChecksum {
    /// Relative path under the target datadir where this file is extracted.
    pub path: String,
    /// Plain file size in bytes.
    pub size: u64,
    /// BLAKE3 checksum of the plain file contents.
    pub blake3: String,
}
