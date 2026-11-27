//! Archive handle: config provisioning, marker lookups and pruning.

use std::sync::Arc;

use bytes::Bytes;
use exoware_sdk::{
    RangeMode,
    prune_policy::{GroupBy, KeysScope, OrderBy, OrderEncoding, PrunePolicy, RetainPolicy},
    selector::Selector,
};
use tracing::info;

use crate::{
    ArchiveConfig, ArchiveError, ArchiveKey, ArchiveMetrics, ArchiveStore, Family, NUMBER_LEN,
    PART_INDEX_LEN, SegmentMarker, StoreSession,
};

/// Default number of segments kept below the earliest served segment before physical pruning.
pub const DEFAULT_PRUNE_GRACE_SEGMENTS: u64 = 2;

/// Name of the prune capture group holding the trailing number.
pub const PRUNE_CAPTURE: &str = "n";

/// Result of [`ExowareArchive::provision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The config row was written.
    Created,
    /// An identical config row already existed.
    AlreadyProvisioned,
}

/// The highest published segment and its marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighestMarker {
    /// Segment number.
    pub segment: u64,
    /// Marker.
    pub marker: SegmentMarker,
}

/// Handle to one archive namespace in an Exoware store.
#[derive(Debug)]
pub struct ExowareArchive<S> {
    /// Store.
    pub store: Arc<S>,
}

impl<S> Clone for ExowareArchive<S> {
    fn clone(&self) -> Self {
        Self { store: Arc::clone(&self.store) }
    }
}

impl<S: ArchiveStore> ExowareArchive<S> {
    /// Wraps a store.
    pub const fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Explicit operator step: writes the config row if absent. An existing row must be
    /// byte-identical. Exporters never create the config implicitly.
    pub async fn provision(
        &self,
        config: &ArchiveConfig,
    ) -> Result<ProvisionOutcome, ArchiveError> {
        config.validate()?;
        let session = self.store.session();
        match session.get(&ArchiveKey::config()).await? {
            Some(raw) if raw.as_ref() == config.encode().as_slice() => {
                Ok(ProvisionOutcome::AlreadyProvisioned)
            }
            Some(raw) => Err(ArchiveError::ConfigMismatch {
                stored: Self::render_config(&raw),
                expected: config.to_string(),
            }),
            None => {
                self.store.put(&[(ArchiveKey::config(), Bytes::from(config.encode()))]).await?;
                info!(config = %config, "provisioned archive config");
                Ok(ProvisionOutcome::Created)
            }
        }
    }

    /// Reads the config row.
    pub async fn read_config(session: &S::Session) -> Result<Option<ArchiveConfig>, ArchiveError> {
        session.get(&ArchiveKey::config()).await?.map(|raw| ArchiveConfig::decode(&raw)).transpose()
    }

    /// Requires the stored config to equal `expected` byte for byte.
    pub async fn require_config(&self, expected: &ArchiveConfig) -> Result<(), ArchiveError> {
        let raw = self
            .store
            .session()
            .get(&ArchiveKey::config())
            .await?
            .ok_or(ArchiveError::ConfigMissing)?;
        if raw.as_ref() != expected.encode().as_slice() {
            return Err(ArchiveError::ConfigMismatch {
                stored: Self::render_config(&raw),
                expected: expected.to_string(),
            });
        }
        Ok(())
    }

    /// Renders a stored config row for errors.
    pub fn render_config(raw: &[u8]) -> String {
        ArchiveConfig::decode(raw)
            .map_or_else(|e| format!("undecodable ({e})"), |config| config.to_string())
    }

    /// Reads the highest marker with a reverse range of limit one, fully consumed.
    pub async fn highest_marker(
        session: &S::Session,
    ) -> Result<Option<HighestMarker>, ArchiveError> {
        let rows = session
            .range(&ArchiveKey::marker(0), &ArchiveKey::marker(u64::MAX), 1, RangeMode::Reverse)
            .await?;
        let Some((key, value)) = rows.into_iter().next() else { return Ok(None) };
        let segment = ArchiveKey::number(&key)
            .filter(|_| ArchiveKey::family(&key) == Some(Family::Marker))
            .ok_or_else(|| ArchiveError::decode(Family::Marker, "malformed marker key"))?;
        Ok(Some(HighestMarker { segment, marker: SegmentMarker::decode(&value)? }))
    }

    /// Reads the inclusive range `[start, end]` forward in pages of `page` rows (at least one),
    /// continuing after the last key while pages come back full. For scans whose row count is
    /// not known in advance, such as block rows with continuation parts.
    pub async fn range_all(
        session: &S::Session,
        start: &Bytes,
        end: &Bytes,
        page: usize,
    ) -> Result<Vec<(Bytes, Bytes)>, ArchiveError> {
        let page = page.max(1);
        let mut out = Vec::new();
        let mut from = start.clone();
        loop {
            let rows = session.range(&from, end, page, RangeMode::Forward).await?;
            let full = rows.len() == page;
            if let Some((last, _)) = rows.last() {
                // The smallest key after `last`.
                let mut next = last.to_vec();
                next.push(0);
                from = next.into();
            }
            out.extend(rows);
            if !full || from > *end {
                return Ok(out);
            }
        }
    }

    /// Reads marker `segment`.
    pub async fn marker(
        session: &S::Session,
        segment: u64,
    ) -> Result<Option<SegmentMarker>, ArchiveError> {
        session
            .get(&ArchiveKey::marker(segment))
            .await?
            .map(|raw| SegmentMarker::decode(&raw))
            .transpose()
    }

    /// Prune policy deleting rows of `family` below segment `cutoff`: logical selector prefix
    /// `gen ‖ family` (the SDK adds the namespace), a binary regex that skips the lookup fields
    /// and captures the trailing 8 bytes, `U64Be` ordering and `GreaterThanOrEqual` retention.
    /// Block families compare block numbers against `N · cutoff`, and their regex also matches
    /// continuation keys, capturing the block number before the 2-byte part index.
    pub fn prune_policy(config: &ArchiveConfig, family: Family, cutoff: u64) -> PrunePolicy {
        let (threshold, part) = if family.keyed_by_block() {
            (config.first_block(cutoff), format!("(?:.{{{PART_INDEX_LEN}}})?"))
        } else {
            (cutoff, String::new())
        };
        let payload_regex = format!(
            "(?s-u)^.{{{}}}(?P<{PRUNE_CAPTURE}>.{{{NUMBER_LEN}}}){part}$",
            family.fields_len()
        );
        PrunePolicy {
            scope: KeysScope {
                selector: Selector {
                    prefix: ArchiveKey::family_prefix(family),
                    payload_regex: payload_regex.into(),
                },
                group_by: GroupBy::default(),
                order_by: Some(OrderBy {
                    capture_group: PRUNE_CAPTURE.into(),
                    encoding: OrderEncoding::U64Be,
                }),
            },
            retain: RetainPolicy::GreaterThanOrEqual { threshold },
        }
    }

    /// Physically deletes rows of every index and block family below the prune cutoff, one
    /// `Prune` call per family. Markers and config are never pruned. Returns the cutoff
    /// segment, if anything was eligible. Safe to repeat: a stalled exporter may reinsert old
    /// rows, so callers run it periodically.
    pub async fn prune(
        &self,
        config: &ArchiveConfig,
        highest: Option<u64>,
        grace: u64,
    ) -> Result<Option<u64>, ArchiveError> {
        let Some(cutoff) = config.prune_cutoff(highest, grace) else { return Ok(None) };
        for family in Family::INDEX.into_iter().chain(Family::BLOCK) {
            self.store.prune(&[Self::prune_policy(config, family, cutoff)]).await?;
        }
        ArchiveMetrics::prune_runs().increment(1);
        info!(cutoff_segment = cutoff, "pruned archive rows");
        Ok(Some(cutoff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SegmentRows,
        test_utils::{ChainFixture, TEST_N, TEST_TIMEOUT, TestSimulator, dump, publish_through},
    };

    /// Block-family prune policies delete continuation rows with their base rows: below the
    /// cutoff both go, at or above it both stay.
    #[tokio::test(flavor = "multi_thread")]
    async fn prune_deletes_continuation_rows() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let sim = TestSimulator::start().await;
            let fixture = ChainFixture::standard();
            let config = fixture.config(TEST_N, 0, None);
            let archive = sim.archive("t-prune-parts/", &config).await;
            let value = Bytes::from_static(b"v");
            let kept = [
                ArchiveKey::block(Family::Body, 16),
                ArchiveKey::block_part(Family::Body, 16, 1),
                ArchiveKey::block_part(Family::Body, 16, u16::MAX),
            ];
            let dropped = [
                ArchiveKey::block(Family::Body, 15),
                ArchiveKey::block_part(Family::Body, 15, 1),
                ArchiveKey::block_part(Family::Body, 15, u16::MAX),
            ];
            let rows: Vec<(Bytes, Bytes)> =
                kept.iter().chain(&dropped).map(|k| (k.clone(), value.clone())).collect();
            archive.store.put(&rows).await.unwrap();
            archive
                .store
                .prune(&[ExowareArchive::<crate::ExowareStore>::prune_policy(
                    &config,
                    Family::Body,
                    2,
                )])
                .await
                .unwrap();
            let after: Vec<Bytes> =
                dump(archive.store.as_ref()).await.into_iter().map(|(k, _)| k).collect();
            let mut expected = vec![ArchiveKey::config()];
            expected.extend(kept);
            assert_eq!(after, expected, "rows of block 15 deleted, rows of block 16 kept");
        })
        .await
        .expect("test timed out");
    }

    /// Test 9 / A2: prune removes exactly the rows of segments below the cutoff in every
    /// family, keeps markers and config, and leaves a neighboring namespace untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn prune_cuts_exactly_below_threshold() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let sim = TestSimulator::start().await;
            let fixture = Arc::new(ChainFixture::standard());
            let config = fixture.config(TEST_N, 0, Some(2));
            let archive = sim.archive("t9/", &config).await;
            let neighbor = sim.archive("t9-x/", &config).await;
            publish_through(&archive, &fixture, config, 4).await;
            publish_through(&neighbor, &fixture, config, 4).await;
            let neighbor_before = dump(neighbor.store.as_ref()).await;
            assert_eq!(
                archive.prune(&config, Some(4), 1).await.unwrap(),
                Some(2),
                "E = 3, grace 1, cutoff 2"
            );

            let mut expected: Vec<(Bytes, Bytes)> =
                vec![(ArchiveKey::config(), Bytes::from(config.encode()))];
            for segment in 0..5 {
                let rows = SegmentRows::build(
                    &config,
                    segment,
                    &fixture.blocks[(segment * TEST_N) as usize..((segment + 1) * TEST_N) as usize],
                )
                .unwrap();
                expected.push((ArchiveKey::marker(segment), Bytes::from(rows.marker.encode())));
                if segment >= 2 {
                    expected.extend(rows.rows);
                }
            }
            expected.sort();
            // A namespace is `[a-z0-9-]+/` and `/` occurs only as its last byte, so no valid
            // namespace is a byte prefix of another ("t9/" and "t9-x/" differ before either
            // ends). An unbounded range over "t9/" therefore returns only this archive's keys;
            // the previous neighbor "t9/x" is now rejected by `ExowareStore::validate_namespace`.
            let after = dump(archive.store.as_ref()).await;
            for family in Family::INDEX.into_iter().chain(Family::BLOCK) {
                let left = |rows: &[(Bytes, Bytes)]| {
                    rows.iter().filter(|(k, _)| ArchiveKey::family(k) == Some(family)).count()
                };
                assert_eq!(left(&after), left(&expected), "{family:?} rows after prune");
            }
            assert_eq!(after.len(), expected.len(), "row count after prune");
            assert!(
                after == expected,
                "store holds exactly config, all markers and segments 2..=4"
            );
            assert!(
                dump(neighbor.store.as_ref()).await == neighbor_before,
                "neighboring namespace untouched"
            );
            assert_eq!(
                archive.prune(&config, Some(4), 1).await.unwrap(),
                Some(2),
                "repeat is idempotent"
            );
            let again = dump(archive.store.as_ref()).await;
            assert!(again == expected, "repeat deletes nothing more");
        })
        .await
        .expect("test timed out");
    }
}
