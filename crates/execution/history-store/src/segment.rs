//! Segment files: reth `NippyJar` static files for one segment of `N` blocks, plus
//! `block_bodies.idx`.
//!
//! The builder drives reth's own `StaticFileProviderRW` in a temporary directory, so the bytes
//! are exactly reth's static-file format and a node can import them with reth's static-file
//! machinery. The archive object names drop the block range from reth's file name, because the
//! range is implied by the segment number.

use std::{collections::BTreeMap, fs, path::Path};

use alloy_primitives::{B256, Bytes};
use base_common_consensus::BasePrimitives;
use reth_provider::{
    ChangeSetReader, HeaderProvider, ReceiptProvider, StaticFileSegment, StorageChangeSetReader,
    TransactionsProvider,
    providers::{StaticFileProvider, StaticFileProviderBuilder, StaticFileWriter},
    static_file::SegmentRangeInclusive,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BlockData, BlockRows, BodyIndices, HistoryStoreError};

/// Prefix shared by every reth static file name.
const STATIC_FILE_PREFIX: &str = "static_file_";

/// Size in bytes of one `block_bodies.idx` entry: first tx number and tx count, both u64 BE.
const BODY_INDEX_ENTRY_LEN: usize = 16;

/// Names and maps the files that make up one archive segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFile;

impl SegmentFile {
    /// Object name of the block body index file.
    pub const BODY_INDICES: &'static str = "block_bodies.idx";

    /// Archive name of a static-file segment, as listed in the TDD layout.
    pub const fn archive_name(segment: StaticFileSegment) -> &'static str {
        match segment {
            StaticFileSegment::Headers => "headers",
            StaticFileSegment::Transactions => "transactions",
            StaticFileSegment::Receipts => "receipts",
            StaticFileSegment::TransactionSenders => "transaction_senders",
            StaticFileSegment::AccountChangeSets => "account_changesets",
            StaticFileSegment::StorageChangeSets => "storage_changesets",
        }
    }

    /// Maps a reth static file name (`static_file_{segment}_{start}_{end}[.ext]`) to its archive
    /// object name (`{archive_name}.{dat|conf|off|csoff}`). Returns `None` for other files, such
    /// as the provider's lock file.
    pub fn object_name(reth_file_name: &str) -> Option<String> {
        if !reth_file_name.starts_with(STATIC_FILE_PREFIX) {
            return None;
        }
        let (stem, ext) = match reth_file_name.split_once('.') {
            Some((stem, ext)) => (stem, ext),
            None => (reth_file_name, "dat"),
        };
        let (segment, _) = StaticFileSegment::parse_filename(stem)?;
        Some(format!("{}.{ext}", Self::archive_name(segment)))
    }

    /// Maps an archive object name back to reth's file name for the given segment range.
    /// Returns `None` for `block_bodies.idx` and unknown names.
    pub fn reth_file_name(object_name: &str, range: &SegmentRangeInclusive) -> Option<String> {
        let (name, ext) = object_name.split_once('.')?;
        let segment = StaticFileSegment::iter().find(|s| Self::archive_name(*s) == name)?;
        let base = segment.filename(range);
        Some(if ext == "dat" { base } else { format!("{base}.{ext}") })
    }
}

/// `block_bodies.idx`: block → (first tx number, tx count) for every block in a segment.
///
/// reth's static-file provider does not serve body indices, so the archive stores them in this
/// fixed-width file: `N` entries of (first tx number u64 BE, tx count u64 BE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyIndicesFile {
    /// One entry per block, in block order.
    pub entries: Vec<BodyIndices>,
}

impl BodyIndicesFile {
    /// Encodes the entries into the fixed-width file format.
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(self.entries.len() * BODY_INDEX_ENTRY_LEN);
        for entry in &self.entries {
            out.extend_from_slice(&entry.first_tx_num.to_be_bytes());
            out.extend_from_slice(&entry.tx_count.to_be_bytes());
        }
        out.into()
    }

    /// Decodes a file that must hold exactly `blocks` entries.
    pub fn decode(segment: u64, raw: &[u8], blocks: u64) -> Result<Self, HistoryStoreError> {
        if raw.len() as u64 != blocks * BODY_INDEX_ENTRY_LEN as u64 {
            return Err(HistoryStoreError::CorruptSegment {
                segment,
                file: SegmentFile::BODY_INDICES.to_string(),
                reason: format!(
                    "expected {} bytes, got {}",
                    blocks * BODY_INDEX_ENTRY_LEN as u64,
                    raw.len()
                ),
            });
        }
        let entries = raw
            .chunks_exact(BODY_INDEX_ENTRY_LEN)
            .map(|chunk| BodyIndices {
                first_tx_num: u64::from_be_bytes(chunk[..8].try_into().expect("8 bytes")),
                tx_count: u64::from_be_bytes(chunk[8..].try_into().expect("8 bytes")),
            })
            .collect();
        Ok(Self { entries })
    }
}

/// Checksum and size of one segment object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// SHA-256 of the object's bytes.
    pub sha256: B256,
    /// Object size in bytes.
    pub size: u64,
}

/// Per-file SHA-256 checksums and sizes of one segment, plus the writer epoch whose
/// `seg/<k>/<epoch>/` directory holds the published objects. Stored in the index under `m‖k`
/// in the same batch as the segment's other entries.
///
/// `NippyJar` data files carry no checksum of their own and the changeset files are not covered
/// by any header root, so readers check every downloaded or disk-cached file against this
/// manifest before opening it. A published segment without a manifest is corrupt. Sizes let a
/// reader reserve disk-cache space before downloading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentManifest {
    /// Segment number `k`.
    pub segment: u64,
    /// First block, `k·N`.
    pub first_block: u64,
    /// Last block, `k·N + N − 1`.
    pub last_block: u64,
    /// Writer epoch whose directory holds the objects.
    pub epoch: u64,
    /// Object name → checksum and size.
    pub files: BTreeMap<String, ManifestEntry>,
}

impl SegmentManifest {
    /// Checks that this manifest describes segment `k` of `n` blocks and that `files` holds
    /// exactly the listed objects with matching checksums.
    pub fn check(
        &self,
        k: u64,
        n: u64,
        files: &BTreeMap<String, Bytes>,
    ) -> Result<(), HistoryStoreError> {
        let corrupt = |file: &str, reason: String| HistoryStoreError::CorruptSegment {
            segment: k,
            file: file.to_string(),
            reason,
        };
        if (self.segment, self.first_block, self.last_block) != (k, k * n, k * n + n - 1) {
            return Err(corrupt(
                "manifest",
                format!(
                    "manifest describes segment {} blocks {}..={}",
                    self.segment, self.first_block, self.last_block
                ),
            ));
        }
        for name in files.keys() {
            if !self.files.contains_key(name) {
                return Err(corrupt(name, "object not in manifest".to_string()));
            }
        }
        for (name, expected) in &self.files {
            let bytes = files.get(name).ok_or_else(|| corrupt(name, "missing".to_string()))?;
            if bytes.len() as u64 != expected.size {
                return Err(corrupt(
                    name,
                    format!("{} bytes != manifest {}", bytes.len(), expected.size),
                ));
            }
            let actual = B256::from_slice(&Sha256::digest(bytes));
            if actual != expected.sha256 {
                return Err(corrupt(
                    name,
                    format!("sha256 {actual} != manifest {}", expected.sha256),
                ));
            }
        }
        Ok(())
    }

    /// Serialises the manifest as JSON.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serialises")
    }

    /// Total bytes of the segment's objects.
    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|entry| entry.size).sum()
    }

    /// Parses a manifest stored for segment `k`.
    pub fn decode(k: u64, raw: &[u8]) -> Result<Self, HistoryStoreError> {
        serde_json::from_slice(raw).map_err(|err| HistoryStoreError::CorruptSegment {
            segment: k,
            file: "manifest".to_string(),
            reason: err.to_string(),
        })
    }
}

/// All objects of one segment, keyed by object name relative to `seg/<k>/<epoch>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFiles {
    /// Segment number `k`.
    pub segment: u64,
    /// Blocks per segment `N`.
    pub blocks_per_segment: u64,
    /// Object name → bytes.
    pub files: BTreeMap<String, Bytes>,
}

impl SegmentFiles {
    /// Block range covered by this segment.
    pub const fn range(&self) -> SegmentRangeInclusive {
        let start = self.segment * self.blocks_per_segment;
        SegmentRangeInclusive::new(start, start + self.blocks_per_segment - 1)
    }

    /// Computes the checksum manifest of these files, uploaded under writer epoch `epoch`.
    pub fn manifest(&self, epoch: u64) -> SegmentManifest {
        let range = self.range();
        SegmentManifest {
            segment: self.segment,
            first_block: range.start(),
            last_block: range.end(),
            epoch,
            files: self
                .files
                .iter()
                .map(|(name, bytes)| {
                    let sha256 = B256::from_slice(&Sha256::digest(bytes));
                    (name.clone(), ManifestEntry { sha256, size: bytes.len() as u64 })
                })
                .collect(),
        }
    }

    /// Writes the static files back under reth's file names into `dir`, ready for a read-only
    /// `StaticFileProvider`. `block_bodies.idx` is not written; use [`Self::body_indices`].
    pub fn write_static_files(&self, dir: &Path) -> Result<(), HistoryStoreError> {
        let range = self.range();
        for (name, bytes) in &self.files {
            if let Some(file_name) = SegmentFile::reth_file_name(name, &range) {
                fs::write(dir.join(file_name), bytes)?;
            }
        }
        Ok(())
    }

    /// Decodes this segment's `block_bodies.idx`.
    pub fn body_indices(&self) -> Result<BodyIndicesFile, HistoryStoreError> {
        let raw = self.files.get(SegmentFile::BODY_INDICES).ok_or_else(|| {
            HistoryStoreError::CorruptSegment {
                segment: self.segment,
                file: SegmentFile::BODY_INDICES.to_string(),
                reason: "missing".to_string(),
            }
        })?;
        BodyIndicesFile::decode(self.segment, raw, self.blocks_per_segment)
    }

    /// Opens a read-only reth static-file provider over this segment's files in `dir`.
    pub fn open_provider(
        &self,
        dir: &Path,
    ) -> Result<StaticFileProvider<BasePrimitives>, HistoryStoreError> {
        self.write_static_files(dir)?;
        Ok(StaticFileProviderBuilder::read_only(dir)
            .with_blocks_per_file(self.blocks_per_segment)
            .build::<BasePrimitives>()?)
    }
}

impl SegmentFiles {
    /// Reads every block back out of the segment's static files, recomputing block hashes from
    /// the headers. Used by the pruner (to recompute index keys) and by the writer (to find the
    /// archive tip).
    pub fn read_blocks(&self) -> Result<Vec<BlockData>, HistoryStoreError> {
        let dir = tempfile::tempdir()?;
        let provider = self.open_provider(dir.path())?;
        let bodies = self.body_indices()?;
        let range = self.range();
        let mut blocks = Vec::with_capacity(bodies.entries.len());
        for (number, body) in (range.start()..=range.end()).zip(bodies.entries) {
            let corrupt = |reason: &str| HistoryStoreError::CorruptSegment {
                segment: self.segment,
                file: format!("block {number}"),
                reason: reason.to_string(),
            };
            let header = provider.header_by_number(number)?.ok_or_else(|| corrupt("header"))?;
            let tx_range = body.first_tx_num..body.next_tx_num();
            let transactions = provider.transactions_by_tx_range(tx_range.clone())?;
            let senders = provider.senders_by_tx_range(tx_range.clone())?;
            let receipts = provider.receipts_by_tx_range(tx_range)?;
            if [transactions.len(), senders.len(), receipts.len()]
                .iter()
                .any(|len| *len as u64 != body.tx_count)
            {
                return Err(corrupt("row count does not match block_bodies.idx"));
            }
            blocks.push(BlockData::new(
                header,
                BlockRows {
                    body_indices: body,
                    transactions,
                    senders,
                    receipts,
                    account_changes: provider.account_block_changeset(number)?,
                    storage_changes: provider.storage_block_changeset(number)?,
                },
            ));
        }
        Ok(blocks)
    }
}

/// Builds the static files of one segment from verified blocks.
#[derive(Debug, Clone, Copy)]
pub struct SegmentBuilder {
    /// Blocks per segment `N`.
    pub blocks_per_segment: u64,
}

impl SegmentBuilder {
    /// Creates a builder for segments of `blocks_per_segment` blocks.
    pub const fn new(blocks_per_segment: u64) -> Self {
        Self { blocks_per_segment }
    }

    /// Builds segment `segment` from exactly `N` contiguous blocks starting at `segment * N`.
    pub fn build(
        &self,
        segment: u64,
        blocks: &[BlockData],
    ) -> Result<SegmentFiles, HistoryStoreError> {
        let first = segment * self.blocks_per_segment;
        let last = first + self.blocks_per_segment - 1;
        let shape_error =
            |reason: String| HistoryStoreError::SegmentShape { segment, first, last, reason };
        if blocks.len() as u64 != self.blocks_per_segment {
            return Err(shape_error(format!("{} blocks", blocks.len())));
        }
        for (offset, block) in blocks.iter().enumerate() {
            if block.number() != first + offset as u64 {
                return Err(shape_error(format!("block {} at offset {offset}", block.number())));
            }
        }

        let dir = tempfile::tempdir()?;
        {
            let provider = StaticFileProviderBuilder::read_write(dir.path())
                .with_blocks_per_file(self.blocks_per_segment)
                .build::<BasePrimitives>()?;
            Self::write_segments(&provider, first, blocks)?;
            provider.commit()?;
        }

        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir.path())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(object) = SegmentFile::object_name(&name) {
                files.insert(object, Bytes::from(fs::read(entry.path())?));
            }
        }
        let bodies =
            BodyIndicesFile { entries: blocks.iter().map(|block| block.body_indices).collect() };
        files.insert(SegmentFile::BODY_INDICES.to_string(), bodies.encode());

        Ok(SegmentFiles { segment, blocks_per_segment: self.blocks_per_segment, files })
    }

    /// Appends every block to the six static-file segments through reth's writers.
    pub fn write_segments(
        provider: &StaticFileProvider<BasePrimitives>,
        first: u64,
        blocks: &[BlockData],
    ) -> Result<(), HistoryStoreError> {
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::Headers)?;
            for block in blocks {
                writer.append_header(&block.header, &block.hash)?;
            }
        }
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::Transactions)?;
            for block in blocks {
                writer.increment_block(block.number())?;
                for (i, tx) in block.transactions.iter().enumerate() {
                    writer.append_transaction(block.body_indices.first_tx_num + i as u64, tx)?;
                }
            }
        }
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::Receipts)?;
            for block in blocks {
                writer.increment_block(block.number())?;
                for (i, receipt) in block.receipts.iter().enumerate() {
                    writer.append_receipt(block.body_indices.first_tx_num + i as u64, receipt)?;
                }
            }
        }
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::TransactionSenders)?;
            for block in blocks {
                writer.increment_block(block.number())?;
                for (i, sender) in block.senders.iter().enumerate() {
                    writer.append_transaction_sender(
                        block.body_indices.first_tx_num + i as u64,
                        sender,
                    )?;
                }
            }
        }
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::AccountChangeSets)?;
            for block in blocks {
                writer.append_account_changeset(block.account_changes.clone(), block.number())?;
            }
        }
        {
            let mut writer = provider.get_writer(first, StaticFileSegment::StorageChangeSets)?;
            for block in blocks {
                writer.append_storage_changeset(block.storage_changes.clone(), block.number())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::test_utils::ChainFixture;

    /// Spike for plan risk 1: a fresh directory accepts all six segments starting at a block
    /// above zero, and the files read back through reth's read-only provider unchanged.
    #[test]
    fn builds_segment_above_zero_and_reads_back() {
        let fixture = ChainFixture::new(8, 3);
        let blocks = &fixture.blocks[4..8];
        let files = SegmentBuilder::new(4).build(1, blocks).expect("segment 1 builds");

        let names: Vec<&str> = files.files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "account_changesets.conf",
                "account_changesets.csoff",
                "account_changesets.dat",
                "account_changesets.off",
                "block_bodies.idx",
                "headers.conf",
                "headers.dat",
                "headers.off",
                "receipts.conf",
                "receipts.dat",
                "receipts.off",
                "storage_changesets.conf",
                "storage_changesets.csoff",
                "storage_changesets.dat",
                "storage_changesets.off",
                "transaction_senders.conf",
                "transaction_senders.dat",
                "transaction_senders.off",
                "transactions.conf",
                "transactions.dat",
                "transactions.off",
            ],
            "segment must contain the six static files plus block_bodies.idx"
        );

        let dir = tempfile::tempdir().unwrap();
        let provider = files.open_provider(dir.path()).expect("read-only provider opens");
        for block in blocks {
            let header = provider.header_by_number(block.number()).unwrap();
            assert_eq!(
                header.as_ref(),
                Some(&block.header),
                "header {} round trips",
                block.number()
            );
            for (i, tx) in block.transactions.iter().enumerate() {
                let tx_num = block.body_indices.first_tx_num + i as u64;
                assert_eq!(
                    provider.transaction_by_id(tx_num).unwrap().as_ref(),
                    Some(tx),
                    "tx {tx_num} round trips"
                );
                assert_eq!(
                    provider.receipt(tx_num).unwrap().as_ref(),
                    Some(&block.receipts[i]),
                    "receipt {tx_num} round trips"
                );
                assert_eq!(
                    provider.transaction_sender(tx_num).unwrap(),
                    Some(block.senders[i]),
                    "sender {tx_num} round trips"
                );
            }
            let mut expected_accounts = block.account_changes.clone();
            expected_accounts.sort_by_key(|change| change.address);
            assert_eq!(
                provider.account_block_changeset(block.number()).unwrap(),
                expected_accounts,
                "account changeset of block {} round trips",
                block.number()
            );
            let mut expected_storage = block.storage_changes.clone();
            expected_storage.sort_by_key(|change| (change.address, change.key));
            assert_eq!(
                provider.storage_block_changeset(block.number()).unwrap(),
                expected_storage,
                "storage changeset of block {} round trips",
                block.number()
            );
        }
        assert_eq!(
            files.body_indices().unwrap().entries,
            blocks.iter().map(|b| b.body_indices).collect::<Vec<_>>(),
            "block_bodies.idx round trips"
        );
    }

    /// Building the same segment twice yields byte-identical files (TDD §8 determinism).
    #[test]
    fn segment_build_is_byte_identical() {
        let fixture = ChainFixture::new(8, 2);
        let builder = SegmentBuilder::new(4);
        let first = builder.build(0, &fixture.blocks[..4]).unwrap();
        let second = builder.build(0, &fixture.blocks[..4]).unwrap();
        let digests = |files: &SegmentFiles| -> Vec<(String, Vec<u8>)> {
            files
                .files
                .iter()
                .map(|(name, bytes)| (name.clone(), Sha256::digest(bytes).to_vec()))
                .collect()
        };
        assert_eq!(digests(&first), digests(&second), "every file must hash identically");
    }

    /// A segment with the wrong block count is rejected before anything is written.
    #[test]
    fn rejects_wrong_block_count() {
        let fixture = ChainFixture::new(8, 1);
        let err = SegmentBuilder::new(4).build(0, &fixture.blocks[..3]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "segment 0 expects blocks 0..=3, got 3 blocks",
            "short segment must be rejected"
        );
    }

    /// Archive object names map to and from reth's static file names.
    #[test]
    fn object_name_mapping_round_trips() {
        let range = SegmentRangeInclusive::new(8, 15);
        assert_eq!(
            SegmentFile::object_name("static_file_transaction-senders_8_15.off").as_deref(),
            Some("transaction_senders.off"),
            "reth name maps to archive name"
        );
        assert_eq!(
            SegmentFile::reth_file_name("transaction_senders.dat", &range).as_deref(),
            Some("static_file_transaction-senders_8_15"),
            "archive name maps back to reth data file"
        );
        assert_eq!(SegmentFile::object_name("lock"), None, "lock file is not uploaded");
    }
}
