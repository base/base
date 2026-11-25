//! On-demand segment audit: rebuild every row from the stored block rows and compare.
//!
//! An overwrite cannot be prevented with unconditional `Put`s, but it is detected here: the
//! audit verifies header linkage and roots, rebuilds the segment deterministically, compares
//! every rebuilt row with the stored one (so a deleted or altered posting is found) and
//! recomputes the digest against the marker.
//!
//! Block families (`0x10`–`0x15`) are keyed by block number, so the audit enumerates every
//! stored key of the segment's block range, through the whole prefix interval of the last
//! block's key, and reports keys that the rebuild does not produce; those rows also enter the
//! digest of the stored rows, so they show as a digest mismatch too.
//! Hash-led families (`0x01`–`0x05`) put the segment number after the hash or address, so the
//! keys of one segment are not a contiguous range: an extra locator or posting can only be found
//! by scanning the whole family. The audit checks every expected hash-led row but cannot report
//! extras there.

use alloy_primitives::{B256, hex};
use bytes::Bytes;
use exoware_sdk::RangeMode;

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, BlockCodec, BlockData,
    BlockVerifier, ExowareArchive, Family, SegmentDigest, SegmentRows, StoreSession,
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
}

impl<S: ArchiveStore> SegmentAuditor<S> {
    /// Creates an auditor.
    pub const fn new(archive: ExowareArchive<S>, config: ArchiveConfig, verifier: BlockVerifier) -> Self {
        Self { archive, config, verifier }
    }

    /// Audits segment `segment`. Store errors are returned; data problems are findings.
    pub async fn audit_segment(&self, segment: u64) -> Result<AuditReport, ArchiveError> {
        let session = self.archive.store.session();
        let mut findings = Vec::new();
        let Some(marker) = ExowareArchive::<S>::marker(&session, segment).await? else {
            findings.push(AuditFinding::MarkerMissing);
            return Ok(Self::report(segment, findings));
        };
        let first = self.config.first_block(segment);
        let last = self.config.last_block(segment);
        let n = self.config.blocks_per_segment as usize;
        let mut unexpected = Vec::new();
        let mut extras: Vec<(Bytes, Bytes)> = Vec::new();
        let mut columns: Vec<Vec<Option<Bytes>>> = Vec::with_capacity(Family::BLOCK.len());
        for family in Family::BLOCK {
            // Block keys have a fixed length, so every key between the last block's key and the
            // next block's key extends the last block's key. Scanning through the next block's
            // key (inclusive) covers the last key's whole prefix interval; that next key itself
            // belongs to the following segment and is skipped. The limit leaves room for
            // malformed keys between the expected ones.
            let end = match last.checked_add(1) {
                Some(next) => ArchiveKey::block(family, next),
                None => {
                    let mut end = ArchiveKey::block(family, last).to_vec();
                    end.extend([0xff; 64]);
                    Bytes::from(end)
                }
            };
            let next_key = last.checked_add(1).map(|next| ArchiveKey::block(family, next));
            let rows = session
                .range(&ArchiveKey::block(family, first), &end, n.saturating_mul(2).saturating_add(1), RangeMode::Forward)
                .await?;
            let mut column = vec![None; n];
            for (key, value) in rows {
                if next_key.as_ref() == Some(&key) {
                    continue;
                }
                match ArchiveKey::number(&key)
                    .filter(|b| (first..=last).contains(b) && ArchiveKey::family(&key) == Some(family))
                {
                    Some(number) => column[(number - first) as usize] = Some(value),
                    None => {
                        unexpected.push(AuditFinding::UnexpectedRow { family, key: hex::encode(&key) });
                        extras.push((key, value));
                    }
                }
            }
            for (i, value) in column.iter().enumerate() {
                if value.is_none() {
                    findings.push(AuditFinding::BlockRowMissing { family, block: first + i as u64 });
                }
            }
            columns.push(column);
        }
        // Unexpected rows do not stop the audit: the segment is still rebuilt, and the rows
        // enter the digest of the stored rows below, so they also show as a digest mismatch.
        // They are reported first.
        let with_unexpected = |findings: Vec<AuditFinding>| {
            let mut all = unexpected.clone();
            all.extend(findings);
            Self::report(segment, all)
        };
        if !findings.is_empty() {
            return Ok(with_unexpected(findings));
        }
        let mut blocks = Vec::with_capacity(n);
        for i in 0..n {
            let number = first + i as u64;
            let cell = |c: usize| columns[c][i].as_deref().unwrap_or_default();
            let decoded = (|| {
                Ok::<_, ArchiveError>(BlockData {
                    header: BlockCodec::decode_header(cell(0))?,
                    body: BlockCodec::decode_body(cell(1))?,
                    receipts: BlockCodec::decode_receipts(cell(2))?,
                    senders: BlockCodec::decode_senders(cell(3))?,
                    account_changes: BlockCodec::decode_account_changes(cell(4))?,
                    storage_changes: BlockCodec::decode_storage_changes(cell(5))?,
                })
            })()
            .and_then(|block| {
                if block.number() != number {
                    return Err(ArchiveError::unavailable(Family::Header, format!("holds header {}", block.number())));
                }
                self.verifier.verify_block(&block)?;
                Ok(block)
            });
            match decoded {
                Ok(block) => blocks.push(block),
                Err(err) => findings.push(AuditFinding::BlockInvalid { block: number, reason: err.to_string() }),
            }
        }
        if !findings.is_empty() {
            return Ok(with_unexpected(findings));
        }
        let headers: Vec<_> = blocks.iter().map(|b| b.header.clone()).collect();
        if let Err(err) = BlockVerifier::verify_linkage(&headers, marker.last_hash) {
            findings.push(AuditFinding::LinkageInvalid { reason: err.to_string() });
            return Ok(with_unexpected(findings));
        }
        let rebuilt = SegmentRows::build(&self.config, segment, &blocks)?;
        if rebuilt.marker != marker {
            findings.push(AuditFinding::MarkerMismatch { stored: marker.digest, rebuilt: rebuilt.marker.digest });
        }
        let keys: Vec<Bytes> = rebuilt.rows.iter().map(|(k, _)| k.clone()).collect();
        let stored = session.get_many(&keys).await?;
        let mut present: Vec<(&[u8], &[u8])> = extras.iter().map(|(k, v)| (k.as_ref(), v.as_ref())).collect();
        for ((key, expected), found) in rebuilt.rows.iter().zip(&stored) {
            let family = ArchiveKey::family(key).unwrap_or(Family::Config);
            match found {
                None => findings.push(AuditFinding::RowMissing { family, key: hex::encode(key) }),
                Some(value) => {
                    if value != expected {
                        findings.push(AuditFinding::RowMismatch { family, key: hex::encode(key) });
                    }
                    present.push((key.as_ref(), value.as_ref()));
                }
            }
        }
        present.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut digest = SegmentDigest::default();
        for (key, value) in present {
            digest.update(key, value);
        }
        let stored_rows = digest.finish();
        if stored_rows != marker.digest {
            findings.push(AuditFinding::DigestMismatch { marker: marker.digest, stored_rows });
        }
        Ok(with_unexpected(findings))
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

    use exoware_sdk::prune_policy::{GroupBy, KeysScope, PrunePolicy, RetainPolicy};
    use exoware_sdk::selector::Selector;

    use super::*;
    use crate::{
        LogFilter,
        test_utils::{ChainFixture, FixtureNode, TEST_N, TEST_TIMEOUT, TestSimulator, publish_through, reader},
    };

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
            archive
                .store
                .prune(&[PrunePolicy {
                    scope: KeysScope {
                        selector: Selector { prefix: key.clone(), payload_regex: "(?s-u)^$".into() },
                        group_by: GroupBy::default(),
                        order_by: None,
                    },
                    retain: RetainPolicy::DropAll,
                }])
                .await
                .unwrap();
            let filter = LogFilter { from: 8, to: 15, addresses: vec![address], topics: Default::default() };
            assert!(!fixture.expected_logs(&filter).is_empty(), "the address logs in segment 1");
            let r = reader(archive.clone(), FixtureNode::new(Arc::clone(&fixture))).await;
            assert_eq!(r.logs(&filter).await.unwrap(), vec![], "a missing posting silently drops logs");

            let auditor = SegmentAuditor::new(archive.clone(), config, fixture.verifier());
            let report = auditor.audit_segment(1).await.unwrap();
            assert_eq!(report.findings.len(), 2, "missing row and digest: {:?}", report.findings);
            assert_eq!(report.findings[0], AuditFinding::RowMissing { family: Family::LogAddress, key: hex::encode(&key) }, "row");
            assert!(matches!(report.findings[1], AuditFinding::DigestMismatch { .. }), "digest");
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
            assert!(auditor.audit_segment(1).await.unwrap().is_clean(), "segment 1 clean before the insert");
            let marker = ExowareArchive::<crate::ExowareStore>::marker(&archive.store.session(), 1).await.unwrap().unwrap();
            // Interior extra (after block 10) and an extra extending the last block's key (15),
            // which sorts past that key.
            for (block, label) in [(10, "interior"), (15, "last block")] {
                let mut extra = ArchiveKey::block(Family::Receipts, block).to_vec();
                extra.push(0);
                archive.store.put(&[(Bytes::from(extra.clone()), Bytes::from_static(b"x"))]).await.unwrap();
                let report = auditor.audit_segment(1).await.unwrap();
                let stored_rows = report.findings.iter().find_map(|f| match f {
                    AuditFinding::DigestMismatch { stored_rows, .. } => Some(*stored_rows),
                    _ => None,
                });
                assert_eq!(
                    report.findings.first(),
                    Some(&AuditFinding::UnexpectedRow { family: Family::Receipts, key: hex::encode(&extra) }),
                    "{label}: the inserted row is reported first"
                );
                assert_eq!(
                    report.findings.last(),
                    Some(&AuditFinding::DigestMismatch { marker: marker.digest, stored_rows: stored_rows.expect("digest mismatch") }),
                    "{label}: the extra enters the stored-row digest"
                );
                assert_ne!(stored_rows, Some(marker.digest), "{label}: digest differs from the marker");
                archive
                    .store
                    .prune(&[PrunePolicy {
                        scope: KeysScope {
                            selector: Selector { prefix: Bytes::from(extra.clone()), payload_regex: "(?s-u)^$".into() },
                            group_by: GroupBy::default(),
                            order_by: None,
                        },
                        retain: RetainPolicy::DropAll,
                    }])
                    .await
                    .unwrap();
                assert!(auditor.audit_segment(1).await.unwrap().is_clean(), "{label}: clean after removing the extra");
            }
            let mut next_segment = ArchiveKey::block(Family::Receipts, 16).to_vec();
            next_segment.push(0);
            archive.store.put(&[(Bytes::from(next_segment), Bytes::from_static(b"x"))]).await.unwrap();
            assert!(auditor.audit_segment(1).await.unwrap().is_clean(), "an extra of segment 2 is not segment 1's");
            assert!(auditor.audit_segment(0).await.unwrap().is_clean(), "segment 0 unaffected");
        })
        .await
        .expect("test timed out");
    }
}
