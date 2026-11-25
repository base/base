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
    /// Builds the rows of segment `segment` from its `N` blocks in order.
    ///
    /// Blocks must be consecutive from the segment's first block and chain by parent hash.
    /// Duplicate keys with different values (impossible for valid chains) are an error.
    pub fn build(
        config: &ArchiveConfig,
        segment: u64,
        blocks: &[BlockData],
    ) -> Result<Self, ArchiveError> {
        let first = config.first_block(segment);
        if blocks.len() as u64 != config.blocks_per_segment {
            return Err(ArchiveError::InvalidBlock {
                block: first,
                reason: format!(
                    "segment {segment} needs {} blocks, got {}",
                    config.blocks_per_segment,
                    blocks.len()
                ),
            });
        }
        let mut builder = RowBuilder::default();
        let mut postings: BTreeMap<Bytes, OffsetSet> = BTreeMap::new();
        let mut parent: Option<B256> = None;
        let mut last_hash = B256::ZERO;
        for (i, block) in blocks.iter().enumerate() {
            let number = first + i as u64;
            if block.number() != number {
                return Err(ArchiveError::InvalidBlock {
                    block: number,
                    reason: format!("got header for block {}", block.number()),
                });
            }
            if let Some(parent) = parent
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
            parent = Some(hash);
            last_hash = hash;
            let offset = config.offset_of(number);

            builder.insert(ArchiveKey::block(Family::Header, number), BlockCodec::encode_header(&block.header))?;
            builder.insert(ArchiveKey::block(Family::Body, number), BlockCodec::encode_body(&block.body))?;
            builder.insert(ArchiveKey::block(Family::Receipts, number), BlockCodec::encode_receipts(&block.receipts))?;
            builder.insert(ArchiveKey::block(Family::Senders, number), BlockCodec::encode_senders(&block.senders))?;
            builder.insert(
                ArchiveKey::block(Family::AccountChanges, number),
                BlockCodec::encode_account_changes(number, &block.account_changes)?,
            )?;
            builder.insert(
                ArchiveKey::block(Family::StorageChanges, number),
                BlockCodec::encode_storage_changes(number, &block.storage_changes)?,
            )?;

            builder.insert(ArchiveKey::block_hash(hash, segment), number.to_be_bytes().to_vec())?;
            for (index, tx) in block.body.transactions.iter().enumerate() {
                let mut locator = Vec::with_capacity(12);
                locator.extend_from_slice(&number.to_be_bytes());
                locator.extend_from_slice(&(index as u32).to_be_bytes());
                builder.insert(ArchiveKey::tx_hash(tx.tx_hash(), segment), locator)?;
            }

            for change in &block.account_changes {
                postings
                    .entry(ArchiveKey::state(&StateKey::account(change.address), segment))
                    .or_default()
                    .insert(offset);
            }
            for change in &block.storage_changes {
                postings
                    .entry(ArchiveKey::state(&StateKey::storage(change.address, change.slot), segment))
                    .or_default()
                    .insert(offset);
            }
            for receipt in &block.receipts {
                for log in receipt.as_receipt().logs.iter() {
                    postings.entry(ArchiveKey::log_address(log.address, segment)).or_default().insert(offset);
                    for (position, topic) in log.topics().iter().enumerate() {
                        postings
                            .entry(ArchiveKey::log_topic(position as u8, *topic, segment))
                            .or_default()
                            .insert(offset);
                    }
                }
            }
        }
        for (key, set) in postings {
            builder.insert(key, set.encode())?;
        }
        let rows: Vec<(Bytes, Bytes)> = builder.rows.into_iter().collect();
        let digest = SegmentDigest::of(rows.iter().map(|(k, v)| (k.as_ref(), v.as_ref())));
        let marker = SegmentMarker { last_hash, row_count: rows.len() as u64, digest };
        let first_parent_hash = blocks[0].header.parent_hash;
        Ok(Self { segment, rows, first_parent_hash, marker })
    }

    /// Total value bytes.
    pub fn value_bytes(&self) -> usize {
        self.rows.iter().map(|(_, v)| v.len()).sum()
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
    use super::*;
    use crate::test_utils::{ChainFixture, TEST_N};

    /// Pinned digest of segment 1 of the standard fixture (Canyon activates inside it). A change
    /// here is a format change and needs a new generation.
    const GOLDEN_SEGMENT_1_DIGEST: &str = "0x7e58a0091fee5b62a0b7b6b959d01fc598a43c5a0b2568f4b5ef0a1bc637dd03";

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
            let other = SegmentRows::build(&config, segment, &b.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize]).unwrap();
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
        let count = |family: Family| rows.rows.iter().filter(|(k, _)| ArchiveKey::family(k) == Some(family)).count();
        for family in Family::BLOCK {
            assert_eq!(count(family), 8, "{family:?} has one row per block");
        }
        assert_eq!(count(Family::BlockHash), 8, "one block locator per block");
        let txs: usize = fixture.blocks[8..16].iter().map(|b| b.body.transactions.len()).sum();
        assert_eq!(count(Family::TxHash), txs, "one tx locator per transaction");
        assert_eq!(count(Family::LogAddress), 3, "three log contracts");
    }

    /// Wrong block counts, gaps and broken parents are rejected.
    #[test]
    fn invalid_segments_rejected() {
        let fixture = ChainFixture::standard();
        let config = fixture.config(TEST_N, 0, None);
        assert!(SegmentRows::build(&config, 1, &fixture.blocks[8..15]).is_err(), "short segment");
        assert!(SegmentRows::build(&config, 1, &fixture.blocks[9..17]).is_err(), "misaligned segment");
        let mut blocks = fixture.blocks[8..16].to_vec();
        blocks[3].header.parent_hash = B256::ZERO;
        assert!(SegmentRows::build(&config, 1, &blocks).is_err(), "broken parent");
    }
}
