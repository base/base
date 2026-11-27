//! On-demand segment audit: rebuild every row from the stored block rows and compare.
//!
//! An overwrite cannot be prevented with unconditional `Put`s, but it is detected here: the
//! audit verifies header linkage and roots, rebuilds the segment deterministically, compares
//! every rebuilt row with the stored one (so a deleted or altered posting is found) and
//! recomputes the digest against the marker.
//!
//! Block families (`0x10`–`0x15`) are keyed by block number, so the audit enumerates every
//! stored key of the segment's block range, through the whole prefix interval of the last
//! block's key, reassembles values from their continuation parts, reports an absent part as a
//! missing row, and reports keys that the rebuild does not produce; those rows also enter the
//! digest of the stored rows, so they show as a digest mismatch too. Blocks are read and
//! rebuilt in batches (`batch_blocks`), and the digest is computed per family as rows are read,
//! the same way the exporter computes the marker's.
//! Hash-led families (`0x01`–`0x05`) put the segment number after the hash or address, so the
//! keys of one segment are not a contiguous range: an extra locator or posting can only be found
//! by scanning the whole family. The audit checks every expected hash-led row but cannot report
//! extras there.

use std::collections::BTreeMap;

use alloy_primitives::{B256, hex};
use bytes::Bytes;

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, BlockCodec, BlockData,
    BlockVerifier, DEFAULT_EXPORT_BATCH_BLOCKS, ExowareArchive, Family, PartAssembly,
    SegmentBuilder, SegmentDigest, StoreSession, ValueParts,
};

/// One problem found by an audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditFinding {
    /// The segment has no marker.
    MarkerMissing,
    /// A block row is absent.
    BlockRowMissing {
        /// Family.
        family: Family,
        /// Block number.
        block: u64,
    },
    /// A block's rows do not decode or fail verification.
    BlockInvalid {
        /// Block number.
        block: u64,
        /// Reason.
        reason: String,
    },
    /// Headers do not link or do not end at the marker's hash.
    LinkageInvalid {
        /// Reason.
        reason: String,
    },
    /// A rebuilt row is absent from the store.
    RowMissing {
        /// Family.
        family: Family,
        /// Hex key.
        key: String,
    },
    /// A stored row differs from the rebuilt one.
    RowMismatch {
        /// Family.
        family: Family,
        /// Hex key.
        key: String,
    },
    /// A stored key in an enumerable family that the rebuild does not produce.
    UnexpectedRow {
        /// Family of the scanned range.
        family: Family,
        /// Hex key.
        key: String,
    },
    /// The rebuilt marker differs from the stored one.
    MarkerMismatch {
        /// Stored digest.
        stored: B256,
        /// Rebuilt digest.
        rebuilt: B256,
    },
    /// The digest of the stored rows differs from the marker's.
    DigestMismatch {
        /// Marker digest.
        marker: B256,
        /// Digest of the stored rows.
        stored_rows: B256,
    },
}

/// Result of auditing one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    /// Segment number.
    pub segment: u64,
    /// Problems found; empty when the segment is intact.
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    /// Whether no problem was found.
    pub const fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Audits published segments.
#[derive(Debug)]
pub struct SegmentAuditor<S> {
    /// Archive handle.
    pub archive: ExowareArchive<S>,
    /// Archive config.
    pub config: ArchiveConfig,
    /// Root and sender checks.
    pub verifier: BlockVerifier,
    /// Blocks read and rebuilt at a time, as the exporter streams them.
    pub batch_blocks: u64,
}

/// One block's value of one family while the audit reads its rows.
#[derive(Debug, Clone)]
enum Cell {
    /// No base row seen.
    Missing,
    /// Base row seen; continuation parts are appended as they arrive.
    Building(PartAssembly),
    /// A finding was recorded for the value.
    Failed,
}

/// State carried across the batches of one audit.
#[derive(Debug)]
struct AuditPass {
    /// Rebuilds the segment's rows and marker batch by batch.
    builder: SegmentBuilder,
    /// Digest of the rows found in the store.
    stored: SegmentDigest,
    /// Hash of the last block of the previous batch.
    parent: Option<B256>,
    /// Data problems found so far (missing, invalid or unlinked block rows).
    findings: Vec<AuditFinding>,
    /// Stored keys of block families that the rebuild does not produce.
    unexpected: Vec<AuditFinding>,
}

impl<S: ArchiveStore> SegmentAuditor<S> {
    /// Creates an auditor that reads [`DEFAULT_EXPORT_BATCH_BLOCKS`] blocks at a time.
    pub const fn new(
        archive: ExowareArchive<S>,
        config: ArchiveConfig,
        verifier: BlockVerifier,
    ) -> Self {
        Self { archive, config, verifier, batch_blocks: DEFAULT_EXPORT_BATCH_BLOCKS }
    }

    /// Audits segment `segment`. Store errors are returned; data problems are findings.
    ///
    /// Block rows are read, verified and rebuilt a batch at a time, so the audit holds one
    /// batch of blocks like the exporter. The stored rows' digest is computed per family as the
    /// rows are read and compared with the marker's at the end.
    pub async fn audit_segment(&self, segment: u64) -> Result<AuditReport, ArchiveError> {
        let session = self.archive.store.session();
        let Some(marker) = ExowareArchive::<S>::marker(&session, segment).await? else {
            return Ok(Self::report(segment, vec![AuditFinding::MarkerMissing]));
        };
        let first = self.config.first_block(segment);
        let last = self.config.last_block(segment);
        let mut pass = AuditPass {
            builder: SegmentBuilder::new(&self.config, segment),
            stored: SegmentDigest::default(),
            parent: None,
            findings: Vec::new(),
            unexpected: Vec::new(),
        };
        let size = self.batch_blocks.max(1);
        for start in (first..=last).step_by(size as usize) {
            self.audit_batch(&session, &mut pass, start, last.min(start + size - 1)).await?;
        }
        // Unexpected rows do not stop the audit: the segment is still rebuilt, and the rows
        // enter the digest of the stored rows below, so they also show as a digest mismatch.
        // They are reported first.
        let AuditPass { builder, mut stored, parent, mut findings, unexpected } = pass;
        let with_unexpected = |findings: Vec<AuditFinding>| {
            let mut all = unexpected.clone();
            all.extend(findings);
            Self::report(segment, all)
        };
        if !findings.is_empty() {
            return Ok(with_unexpected(findings));
        }
        if parent != Some(marker.last_hash) {
            findings.push(AuditFinding::LinkageInvalid {
                reason: format!(
                    "segment's last header does not hash to the marker's {}",
                    marker.last_hash
                ),
            });
            return Ok(with_unexpected(findings));
        }
        let rebuilt = builder.finish()?;
        if rebuilt.marker != marker {
            findings.push(AuditFinding::MarkerMismatch {
                stored: marker.digest,
                rebuilt: rebuilt.marker.digest,
            });
        }
        let keys: Vec<Bytes> = rebuilt.index_rows.iter().map(|(k, _)| k.clone()).collect();
        let found = session.get_many(&keys).await?;
        for ((key, expected), found) in rebuilt.index_rows.iter().zip(&found) {
            let family = ArchiveKey::family(key).unwrap_or(Family::Config);
            match found {
                None => findings.push(AuditFinding::RowMissing { family, key: hex::encode(key) }),
                Some(value) => {
                    if value != expected {
                        findings.push(AuditFinding::RowMismatch { family, key: hex::encode(key) });
                    }
                    stored.update(family, key, value);
                }
            }
        }
        let stored_rows = stored.finish();
        if stored_rows != marker.digest {
            findings.push(AuditFinding::DigestMismatch { marker: marker.digest, stored_rows });
        }
        Ok(with_unexpected(findings))
    }

    /// Reads, checks and rebuilds blocks `from..=to` of the segment.
    async fn audit_batch(
        &self,
        session: &S::Session,
        pass: &mut AuditPass,
        from: u64,
        to: u64,
    ) -> Result<(), ArchiveError> {
        let n = (to - from + 1) as usize;
        let parts = ValueParts::default();
        let mut columns: Vec<Vec<Option<Bytes>>> = Vec::with_capacity(Family::BLOCK.len());
        // Stored block rows of the batch by key, base and continuation rows alike, to compare
        // with the rebuild.
        let mut stored_rows: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        for family in Family::BLOCK {
            // Every key between block `to`'s key and the next block's key extends block `to`'s
            // key: its continuation parts and any malformed key. Scanning through the next
            // block's key (inclusive) covers that whole prefix interval; the next key itself
            // belongs to the next batch or segment and is skipped. The scan pages, so any number
            // of continuation parts and malformed keys is read.
            let next_key = to.checked_add(1).map(|next| ArchiveKey::block(family, next));
            let end = next_key.clone().unwrap_or_else(|| {
                let mut end = ArchiveKey::block(family, to).to_vec();
                end.extend([0xff; 64]);
                Bytes::from(end)
            });
            let rows = ExowareArchive::<S>::range_all(
                session,
                &ArchiveKey::block(family, from),
                &end,
                n.saturating_mul(2).saturating_add(1),
            )
            .await?;
            let mut cells: Vec<Cell> = vec![Cell::Missing; n];
            for (key, value) in rows {
                if next_key.as_ref() == Some(&key) {
                    continue;
                }
                // Rows of one family arrive in key order, batch after batch.
                pass.stored.update(family, &key, &value);
                let Some(number) = ArchiveKey::number(&key).filter(|b| {
                    (from..=to).contains(b) && ArchiveKey::family(&key) == Some(family)
                }) else {
                    pass.unexpected
                        .push(AuditFinding::UnexpectedRow { family, key: hex::encode(&key) });
                    continue;
                };
                let cell = &mut cells[(number - from) as usize];
                match (ArchiveKey::part(&key), &mut *cell) {
                    (None, _) => match parts.assemble(family, number, value.clone()) {
                        Ok(assembly) => *cell = Cell::Building(assembly),
                        Err(err) => {
                            pass.findings.push(AuditFinding::BlockInvalid {
                                block: number,
                                reason: err.to_string(),
                            });
                            *cell = Cell::Failed;
                        }
                    },
                    (Some(_), Cell::Building(assembly)) if assembly.next_part().is_none() => {
                        // The rebuild never produces a part after a short one.
                        pass.unexpected
                            .push(AuditFinding::UnexpectedRow { family, key: hex::encode(&key) });
                        continue;
                    }
                    (Some(part), Cell::Building(assembly))
                        if assembly.next_part() != Some(part) =>
                    {
                        let missing = assembly.next_key().unwrap_or_default();
                        pass.findings
                            .push(AuditFinding::RowMissing { family, key: hex::encode(&missing) });
                        *cell = Cell::Failed;
                    }
                    (Some(part), Cell::Building(assembly)) => {
                        if let Err(err) = assembly.push(part, &value) {
                            pass.findings.push(AuditFinding::BlockInvalid {
                                block: number,
                                reason: err.to_string(),
                            });
                            *cell = Cell::Failed;
                        }
                    }
                    // A part without its base row: the base row is reported missing below.
                    (Some(_), Cell::Missing | Cell::Failed) => {}
                }
                stored_rows.insert(key, value);
            }
            let mut column = Vec::with_capacity(n);
            for (i, cell) in cells.into_iter().enumerate() {
                let block = from + i as u64;
                column.push(match cell {
                    Cell::Missing => {
                        pass.findings.push(AuditFinding::BlockRowMissing { family, block });
                        None
                    }
                    Cell::Failed => None,
                    Cell::Building(assembly) => match assembly.next_key() {
                        Some(missing) => {
                            pass.findings.push(AuditFinding::RowMissing {
                                family,
                                key: hex::encode(&missing),
                            });
                            None
                        }
                        None => assembly.finish().ok(),
                    },
                });
            }
            columns.push(column);
        }
        if !pass.findings.is_empty() {
            return Ok(());
        }
        let mut blocks = Vec::with_capacity(n);
        for (i, number) in (from..=to).enumerate() {
            let cell = |family: usize| columns[family][i].as_deref().unwrap_or_default();
            match self.decode_block(number, cell) {
                Ok(block) => blocks.push(block),
                Err(err) => pass
                    .findings
                    .push(AuditFinding::BlockInvalid { block: number, reason: err.to_string() }),
            }
        }
        if !pass.findings.is_empty() {
            return Ok(());
        }
        for block in &blocks {
            if pass.parent.is_some_and(|parent| block.header.parent_hash != parent) {
                pass.findings.push(AuditFinding::LinkageInvalid {
                    reason: format!(
                        "header {} does not link to header {}",
                        block.number(),
                        block.number() - 1
                    ),
                });
                return Ok(());
            }
            pass.parent = Some(block.hash());
        }
        let rebuilt = pass.builder.push_batch(&blocks)?;
        for (key, expected) in rebuilt {
            let family = ArchiveKey::family(&key).unwrap_or(Family::Config);
            match stored_rows.get(&key) {
                None => {
                    pass.findings.push(AuditFinding::RowMissing { family, key: hex::encode(&key) })
                }
                Some(value) if *value != expected => {
                    pass.findings
                        .push(AuditFinding::RowMismatch { family, key: hex::encode(&key) });
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Decodes and verifies block `number` from its six block-family values.
    fn decode_block<'a>(
        &self,
        number: u64,
        cell: impl Fn(usize) -> &'a [u8],
    ) -> Result<BlockData, ArchiveError> {
        let block = BlockData {
            header: BlockCodec::decode_header(cell(0))?,
            body: BlockCodec::decode_body(cell(1))?,
            receipts: BlockCodec::decode_receipts(cell(2))?,
            senders: BlockCodec::decode_senders(cell(3))?,
            account_changes: BlockCodec::decode_account_changes(cell(4))?,
            storage_changes: BlockCodec::decode_storage_changes(cell(5))?,
        };
        if block.number() != number {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("holds header {}", block.number()),
            ));
        }
        self.verifier.verify_block(&block)?;
        Ok(block)
    }

    /// Builds a report, counting a dirty segment as a fault.
    pub fn report(segment: u64, findings: Vec<AuditFinding>) -> AuditReport {
        if !findings.is_empty() {
            ArchiveMetrics::faults(Family::Marker.name()).increment(1);
        }
        AuditReport { segment, findings }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use exoware_sdk::{
        prune_policy::{GroupBy, KeysScope, PrunePolicy, RetainPolicy},
        selector::Selector,
    };

    use super::*;
    use crate::{
        LogFilter, SegmentRows,
        test_utils::{
            ChainFixture, FixtureNode, TEST_N, TEST_TIMEOUT, TestSimulator, dump, publish_through,
            reader,
        },
    };

    /// Digest of segment 1's rows rebuilt from the fixture, with `edit` applied to the key
    /// set first; computed from raw rows, independently of the auditor.
    fn rebuilt_digest(
        fixture: &ChainFixture,
        config: &ArchiveConfig,
        edit: impl FnOnce(&mut std::collections::BTreeMap<Bytes, Bytes>),
    ) -> B256 {
        let first = (config.first_block(1)) as usize;
        let built = SegmentRows::build(config, 1, &fixture.blocks[first..first + TEST_N as usize])
            .expect("segment 1 builds");
        let mut rows: std::collections::BTreeMap<Bytes, Bytes> = built.rows.into_iter().collect();
        edit(&mut rows);
        let mut digest = SegmentDigest::default();
        for (key, value) in &rows {
            // Malformed extras have no well-formed family; their family byte decides the digest.
            let family = Family::from_byte(key[1]).expect("family byte");
            digest.update(family, key, value);
        }
        digest.finish()
    }

    /// A3: a deleted posting goes unnoticed by log reads (postings only detect false
    /// positives) but the audit reports the missing row and the digest mismatch.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleted_posting_is_found_by_audit() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-audit/", &config).await;
            publish_through(&archive, &fixture, config, 1).await;
            let address = fixture.contracts[2];
            let key = ArchiveKey::log_address(address, 1);
            let filter =
                LogFilter { from: 8, to: 15, addresses: vec![address], topics: Default::default() };
            assert!(!fixture.expected_logs(&filter).is_empty(), "the address logs in segment 1");
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            let logs = |found: Vec<crate::ArchivedLog>| {
                found.into_iter().map(|l| (l.block_number, l.log_index, l.log)).collect::<Vec<_>>()
            };
            assert_eq!(
                logs(r.logs(&filter).await.unwrap()),
                fixture.expected_logs(&filter),
                "before the deletion the posting serves every log"
            );
            let marker = ExowareArchive::<crate::ExowareStore>::marker(&archive.store.session(), 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                marker.digest,
                rebuilt_digest(&fixture, &config, |_| {}),
                "the marker digest is the fixture's rows' digest"
            );
            assert!(
                dump(archive.store.as_ref()).await.iter().any(|(k, _)| *k == key),
                "the posting exists before the deletion"
            );
            archive
                .store
                .prune(&[PrunePolicy {
                    scope: KeysScope {
                        selector: Selector {
                            prefix: key.clone(),
                            payload_regex: "(?s-u)^$".into(),
                        },
                        group_by: GroupBy::default(),
                        order_by: None,
                    },
                    retain: RetainPolicy::DropAll,
                }])
                .await
                .unwrap();
            assert_eq!(
                r.logs(&filter).await.unwrap(),
                vec![],
                "a missing posting silently drops logs"
            );

            let auditor = SegmentAuditor::new(archive.clone(), config, fixture.verifier());
            let report = auditor.audit_segment(1).await.unwrap();
            let without_posting = rebuilt_digest(&fixture, &config, |rows| {
                rows.remove(&key).expect("the posting is a segment row");
            });
            assert_ne!(without_posting, marker.digest, "the deletion changes the digest");
            assert_eq!(
                report.findings,
                vec![
                    AuditFinding::RowMissing { family: Family::LogAddress, key: hex::encode(&key) },
                    AuditFinding::DigestMismatch {
                        marker: marker.digest,
                        stored_rows: without_posting
                    },
                ],
                "the missing row, then the digest of the rows without it"
            );
            assert!(auditor.audit_segment(0).await.unwrap().is_clean(), "segment 0 clean");
        })
        .await
        .expect("test timed out");
    }

    /// B7: an extra row inside a block family's key range is reported as unexpected.
    #[tokio::test(flavor = "multi_thread")]
    async fn extra_block_family_row_is_found_by_audit() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-audit-extra/", &config).await;
            publish_through(&archive, &fixture, config, 1).await;
            let auditor = SegmentAuditor::new(archive.clone(), config, fixture.verifier());
            assert!(
                auditor.audit_segment(1).await.unwrap().is_clean(),
                "segment 1 clean before the insert"
            );
            let marker = ExowareArchive::<crate::ExowareStore>::marker(&archive.store.session(), 1)
                .await
                .unwrap()
                .unwrap();
            // Interior extra (after block 10) and an extra extending the last block's key (15),
            // which sorts past that key.
            for (block, label) in [(10, "interior"), (15, "last block")] {
                let mut extra = ArchiveKey::block(Family::Receipts, block).to_vec();
                extra.push(0);
                archive
                    .store
                    .put(&[(Bytes::from(extra.clone()), Bytes::from_static(b"x"))])
                    .await
                    .unwrap();
                let report = auditor.audit_segment(1).await.unwrap();
                let with_extra = rebuilt_digest(&fixture, &config, |rows| {
                    rows.insert(Bytes::from(extra.clone()), Bytes::from_static(b"x"));
                });
                assert_ne!(with_extra, marker.digest, "{label}: the extra changes the digest");
                assert_eq!(
                    report.findings,
                    vec![
                        AuditFinding::UnexpectedRow {
                            family: Family::Receipts,
                            key: hex::encode(&extra)
                        },
                        AuditFinding::DigestMismatch {
                            marker: marker.digest,
                            stored_rows: with_extra
                        },
                    ],
                    "{label}: exactly the inserted row, then the digest of the rows with it"
                );
                archive
                    .store
                    .prune(&[PrunePolicy {
                        scope: KeysScope {
                            selector: Selector {
                                prefix: Bytes::from(extra.clone()),
                                payload_regex: "(?s-u)^$".into(),
                            },
                            group_by: GroupBy::default(),
                            order_by: None,
                        },
                        retain: RetainPolicy::DropAll,
                    }])
                    .await
                    .unwrap();
                assert!(
                    auditor.audit_segment(1).await.unwrap().is_clean(),
                    "{label}: clean after removing the extra"
                );
            }
            let mut next_segment = ArchiveKey::block(Family::Receipts, 16).to_vec();
            next_segment.push(0);
            archive
                .store
                .put(&[(Bytes::from(next_segment), Bytes::from_static(b"x"))])
                .await
                .unwrap();
            assert!(
                auditor.audit_segment(1).await.unwrap().is_clean(),
                "an extra of segment 2 is not segment 1's"
            );
            assert!(auditor.audit_segment(0).await.unwrap().is_clean(), "segment 0 unaffected");
        })
        .await
        .expect("test timed out");
    }
}
