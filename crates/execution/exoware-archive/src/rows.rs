//! Deterministic segment rows: a pure function of the segment's block data.

use std::collections::{BTreeMap, btree_map::Entry};

use alloy_primitives::{B256, hex};
use bytes::Bytes;

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, BlockCodec, BlockData, Family, OffsetSet,
    SegmentDigest, SegmentMarker, StateKey,
};

/// Every row of one segment, sorted by key, plus its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRows {
    /// Segment number.
    pub segment: u64,
    /// Rows in key order.
    pub rows: Vec<(Bytes, Bytes)>,
    /// Parent hash of the segment's first block; must equal marker `segment − 1`'s last hash.
    pub first_parent_hash: B256,
    /// Marker for these rows.
    pub marker: SegmentMarker,
}

impl SegmentRows {
    /// Builds the rows of segment `segment` from its `N` blocks in order, in one batch.
    ///
    /// Blocks must be consecutive from the segment's first block and chain by parent hash.
    /// Duplicate keys with different values (impossible for valid chains) are an error. The
    /// exporter streams the same rows through [`SegmentBuilder`] instead, a batch at a time.
    pub fn build(
        config: &ArchiveConfig,
        segment: u64,
        blocks: &[BlockData],
    ) -> Result<Self, ArchiveError> {
        let mut builder = SegmentBuilder::new(config, segment);
        let block_rows = builder.push_batch(blocks)?;
        let tail = builder.finish()?;
        // Index families (0x01–0x05) sort before block families (0x10–0x15).
        let mut rows = tail.index_rows;
        rows.extend(block_rows);
        Ok(Self { segment, rows, first_parent_hash: tail.first_parent_hash, marker: tail.marker })
    }

    /// Total value bytes.
    pub fn value_bytes(&self) -> usize {
        self.rows.iter().map(|(_, v)| v.len()).sum()
    }
}

/// What a [`SegmentBuilder`] holds after the last block: the index rows and the marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentTail {
    /// Block-hash and transaction-hash locators and postings (families `0x01`–`0x05`), in key
    /// order.
    pub index_rows: Vec<(Bytes, Bytes)>,
    /// Parent hash of the segment's first block.
    pub first_parent_hash: B256,
    /// Marker over every row of the segment.
    pub marker: SegmentMarker,
}

/// Builds one segment's rows from its blocks in consecutive batches.
///
/// Each batch returns its block-family rows (`0x10`–`0x15`) for immediate writing and folds
/// them into the digest, so only one batch of blocks is held at a time. Index rows (locators
/// and postings, `0x01`–`0x05`) and the digest state accumulate until [`Self::finish`]. The
/// result does not depend on how the blocks are split into batches.
#[derive(Debug)]
pub struct SegmentBuilder {
    /// Archive config.
    pub config: ArchiveConfig,
    /// Segment number.
    pub segment: u64,
    /// Next expected block number.
    pub next: u64,
    /// Hash of the last block pushed.
    pub parent: Option<B256>,
    /// Parent hash of the segment's first block.
    pub first_parent_hash: Option<B256>,
    /// Locator rows.
    pub index: RowBuilder,
    /// Postings by key.
    pub postings: BTreeMap<Bytes, OffsetSet>,
    /// Running digest.
    pub digest: SegmentDigest,
    /// Block-family rows emitted so far.
    pub block_rows: u64,
}

impl SegmentBuilder {
    /// Starts segment `segment`.
    pub fn new(config: &ArchiveConfig, segment: u64) -> Self {
        Self {
            config: *config,
            segment,
            next: config.first_block(segment),
            parent: None,
            first_parent_hash: None,
            index: RowBuilder::default(),
            postings: BTreeMap::new(),
            digest: SegmentDigest::default(),
            block_rows: 0,
        }
    }

    /// Adds the next consecutive blocks and returns their block-family rows in key order.
    pub fn push_batch(
        &mut self,
        blocks: &[BlockData],
    ) -> Result<Vec<(Bytes, Bytes)>, ArchiveError> {
        let segment = self.segment;
        let last = self.config.last_block(segment);
        let mut rows = RowBuilder::default();
        for block in blocks {
            let number = self.next;
            if number > last {
                return Err(ArchiveError::InvalidBlock {
                    block: number,
                    reason: format!(
                        "segment {segment} has only {} blocks",
                        self.config.blocks_per_segment
                    ),
                });
            }
            if block.number() != number {
                return Err(ArchiveError::InvalidBlock {
                    block: number,
                    reason: format!("got header for block {}", block.number()),
                });
            }
            if let Some(parent) = self.parent
                && block.header.parent_hash != parent
            {
                return Err(ArchiveError::InvalidBlock {
                    block: number,
                    reason: "parent hash does not match the previous block".to_string(),
                });
            }
            if block.senders.len() != block.body.transactions.len()
                || block.receipts.len() != block.body.transactions.len()
            {
                return Err(ArchiveError::InvalidBlock {
                    block: number,
                    reason: "sender or receipt count differs from transaction count".to_string(),
                });
            }
            let hash = block.hash();
            self.first_parent_hash.get_or_insert(block.header.parent_hash);
            self.parent = Some(hash);
            self.next += 1;
            let offset = self.config.offset_of(number);

            rows.insert(
                ArchiveKey::block(Family::Header, number),
                BlockCodec::encode_header(&block.header),
            )?;
            rows.insert(
                ArchiveKey::block(Family::Body, number),
                BlockCodec::encode_body(&block.body),
            )?;
            rows.insert(
                ArchiveKey::block(Family::Receipts, number),
                BlockCodec::encode_receipts(&block.receipts),
            )?;
            rows.insert(
                ArchiveKey::block(Family::Senders, number),
                BlockCodec::encode_senders(&block.senders),
            )?;
            rows.insert(
                ArchiveKey::block(Family::AccountChanges, number),
                BlockCodec::encode_account_changes(number, &block.account_changes)?,
            )?;
            rows.insert(
                ArchiveKey::block(Family::StorageChanges, number),
                BlockCodec::encode_storage_changes(number, &block.storage_changes)?,
            )?;

            self.index
                .insert(ArchiveKey::block_hash(hash, segment), number.to_be_bytes().to_vec())?;
            for (index, tx) in block.body.transactions.iter().enumerate() {
                let mut locator = Vec::with_capacity(12);
                locator.extend_from_slice(&number.to_be_bytes());
                locator.extend_from_slice(&(index as u32).to_be_bytes());
                self.index.insert(ArchiveKey::tx_hash(tx.tx_hash(), segment), locator)?;
            }

            for change in &block.account_changes {
                self.postings
                    .entry(ArchiveKey::state(&StateKey::account(change.address), segment))
                    .or_default()
                    .insert(offset);
            }
            for change in &block.storage_changes {
                self.postings
                    .entry(ArchiveKey::state(
                        &StateKey::storage(change.address, change.slot),
                        segment,
                    ))
                    .or_default()
                    .insert(offset);
            }
            for receipt in &block.receipts {
                for log in &receipt.as_receipt().logs {
                    self.postings
                        .entry(ArchiveKey::log_address(log.address, segment))
                        .or_default()
                        .insert(offset);
                    for (position, topic) in log.topics().iter().enumerate() {
                        self.postings
                            .entry(ArchiveKey::log_topic(position as u8, *topic, segment))
                            .or_default()
                            .insert(offset);
                    }
                }
            }
        }
        let rows: Vec<(Bytes, Bytes)> = rows.rows.into_iter().collect();
        // Within a batch rows are ordered by family, then block; across batches each family's
        // blocks keep ascending, so every family digest sees its rows in key order.
        for (key, value) in &rows {
            self.digest.update(ArchiveKey::family(key).unwrap_or(Family::Config), key, value);
        }
        self.block_rows += rows.len() as u64;
        Ok(rows)
    }

    /// Completes the segment: every block must have been pushed. Returns the index rows and
    /// the marker.
    pub fn finish(self) -> Result<SegmentTail, ArchiveError> {
        let first = self.config.first_block(self.segment);
        let pushed = self.next - first;
        let (Some(last_hash), Some(first_parent_hash)) = (self.parent, self.first_parent_hash)
        else {
            return Err(Self::short(self.segment, &self.config, pushed));
        };
        if pushed != self.config.blocks_per_segment {
            return Err(Self::short(self.segment, &self.config, pushed));
        }
        let Self { mut index, postings, mut digest, block_rows, .. } = self;
        for (key, set) in postings {
            index.insert(key, set.encode())?;
        }
        let index_rows: Vec<(Bytes, Bytes)> = index.rows.into_iter().collect();
        for (key, value) in &index_rows {
            digest.update(ArchiveKey::family(key).unwrap_or(Family::Config), key, value);
        }
        let marker = SegmentMarker {
            last_hash,
            row_count: block_rows + index_rows.len() as u64,
            digest: digest.finish(),
        };
        Ok(SegmentTail { index_rows, first_parent_hash, marker })
    }

    /// The error for a segment that did not get all its blocks.
    fn short(segment: u64, config: &ArchiveConfig, pushed: u64) -> ArchiveError {
        ArchiveError::InvalidBlock {
            block: config.first_block(segment) + pushed,
            reason: format!(
                "segment {segment} needs {} blocks, got {pushed}",
                config.blocks_per_segment
            ),
        }
    }
}

/// Collects rows, rejecting conflicting duplicates.
#[derive(Debug, Default)]
pub struct RowBuilder {
    /// Rows by key.
    pub rows: BTreeMap<Bytes, Bytes>,
}

impl RowBuilder {
    /// Inserts a row; an identical duplicate is a no-op, a conflicting one an error.
    pub fn insert(&mut self, key: Bytes, value: Vec<u8>) -> Result<(), ArchiveError> {
        match self.rows.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(value.into());
                Ok(())
            }
            Entry::Occupied(slot) if slot.get().as_ref() == value.as_slice() => Ok(()),
            Entry::Occupied(slot) => Err(ArchiveError::InvalidBlock {
                block: ArchiveKey::number(slot.key()).unwrap_or_default(),
                reason: format!("conflicting values for key {}", hex::encode(slot.key())),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use sha2::Digest;

    use super::*;
    use crate::test_utils::{ChainFixture, TEST_N};

    /// Pinned digest of segment 1 of the standard fixture (Canyon activates inside it): SHA-256
    /// over the per-family digests in family-byte order. A change here is a format change and
    /// needs a new generation.
    const GOLDEN_SEGMENT_1_DIGEST: &str =
        "0x3f7755aace8eb0de574825c0b04e1cfe575a3b27a145d45c5a99d3962c6a8e79";

    /// Building twice, including from an independently built fixture, gives byte-identical
    /// rows and digest.
    #[test]
    fn rows_are_deterministic() {
        let a = ChainFixture::standard();
        let b = ChainFixture::standard();
        let config = a.config(TEST_N, 0, None);
        for segment in 0..5 {
            let blocks = &a.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize];
            let first = SegmentRows::build(&config, segment, blocks).unwrap();
            let again = SegmentRows::build(&config, segment, blocks).unwrap();
            let other = SegmentRows::build(
                &config,
                segment,
                &b.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize],
            )
            .unwrap();
            assert_eq!(first, again, "segment {segment} rebuilt identically");
            assert_eq!(first, other, "segment {segment} identical from an independent source");
            assert_eq!(first.marker.row_count, first.rows.len() as u64, "row count");
            assert!(first.rows.windows(2).all(|w| w[0].0 < w[1].0), "rows sorted and unique");
        }
    }

    /// Golden digest across the Canyon boundary, plus the exact row count by family.
    #[test]
    fn golden_segment_across_canyon() {
        let fixture = ChainFixture::standard();
        let config = fixture.config(TEST_N, 0, None);
        let rows = SegmentRows::build(&config, 1, &fixture.blocks[8..16]).unwrap();
        assert_eq!(rows.marker.last_hash, fixture.blocks[15].hash(), "last hash");
        assert_eq!(rows.marker.digest.to_string(), GOLDEN_SEGMENT_1_DIGEST, "golden digest");
        let mut outer = sha2::Sha256::new();
        for family in crate::DIGEST_FAMILIES {
            let mut digest = crate::FamilyDigest::default();
            for (key, value) in
                rows.rows.iter().filter(|(k, _)| ArchiveKey::family(k) == Some(family))
            {
                digest.update(key, value);
            }
            outer.update(digest.finish());
        }
        assert_eq!(
            rows.marker.digest,
            B256::from_slice(&outer.finalize()),
            "the digest is SHA-256 over the per-family digests, recomputed from the rows"
        );
        let count = |family: Family| {
            rows.rows.iter().filter(|(k, _)| ArchiveKey::family(k) == Some(family)).count()
        };
        for family in Family::BLOCK {
            assert_eq!(count(family), 8, "{family:?} has one row per block");
        }
        assert_eq!(count(Family::BlockHash), 8, "one block locator per block");
        let txs: usize = fixture.blocks[8..16].iter().map(|b| b.body.transactions.len()).sum();
        assert_eq!(count(Family::TxHash), txs, "one tx locator per transaction");
        assert_eq!(count(Family::LogAddress), 3, "three log contracts");
        assert_eq!(
            count(Family::LogTopic),
            10,
            "ten log topic postings (pinned with the golden digest)"
        );
        assert_eq!(
            count(Family::StateHistory),
            10,
            "ten state postings (pinned with the golden digest)"
        );
    }

    /// Splitting a segment into batches of any size gives the same rows and marker as one
    /// batch.
    #[test]
    fn batches_do_not_change_rows() {
        let fixture = ChainFixture::standard();
        let config = fixture.config(TEST_N, 0, None);
        let blocks = &fixture.blocks[8..16];
        let whole = SegmentRows::build(&config, 1, blocks).unwrap();
        for size in 1..=8 {
            let mut builder = SegmentBuilder::new(&config, 1);
            let mut block_rows = Vec::new();
            for batch in blocks.chunks(size) {
                block_rows.extend(builder.push_batch(batch).unwrap());
            }
            let tail = builder.finish().unwrap();
            assert_eq!(tail.marker, whole.marker, "batch size {size}: same marker");
            let mut rows = tail.index_rows;
            rows.extend(block_rows);
            // Batches emit each family's rows in block order, so only the merged, sorted set
            // is in global key order.
            rows.sort();
            assert_eq!(rows, whole.rows, "batch size {size}: same rows");
        }
        let mut builder = SegmentBuilder::new(&config, 1);
        builder.push_batch(blocks).unwrap();
        assert!(builder.push_batch(&fixture.blocks[16..17]).is_err(), "a ninth block is refused");
    }

    /// Wrong block counts, gaps and broken parents are rejected.
    #[test]
    fn invalid_segments_rejected() {
        let fixture = ChainFixture::standard();
        let config = fixture.config(TEST_N, 0, None);
        assert!(SegmentRows::build(&config, 1, &fixture.blocks[8..15]).is_err(), "short segment");
        assert!(
            SegmentRows::build(&config, 1, &fixture.blocks[9..17]).is_err(),
            "misaligned segment"
        );
        let mut blocks = fixture.blocks[8..16].to_vec();
        blocks[3].header.parent_hash = B256::ZERO;
        assert!(SegmentRows::build(&config, 1, &blocks).is_err(), "broken parent");
    }
}
