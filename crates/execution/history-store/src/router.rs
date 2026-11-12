//! Request routing between the node's local database and the archive (TDD §5).

use crate::{Catalog, HistoryStoreError};

/// Where a request at one block is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The node's own database holds the block.
    Local,
    /// The archive holds the block (`E ≤ n ≤ H`).
    Archive,
    /// The block is older than the archive's earliest retained block `E`.
    HistoryPruned,
    /// Neither the node nor the archive can serve the block (above `H`, or catalog unknown).
    Unavailable,
}

/// The archive bounds a single request resolves once and uses throughout (TDD §7: readers
/// resolve `H` once per request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveSnapshot {
    /// Highest published block, `None` before the first segment.
    pub h: Option<u64>,
    /// Earliest retained block.
    pub e: u64,
    /// Blocks per segment.
    pub blocks_per_segment: u64,
}

impl ArchiveSnapshot {
    /// Takes the bounds from a catalog.
    pub fn from_catalog(catalog: &Catalog) -> Result<Self, HistoryStoreError> {
        Ok(Self { h: catalog.h, e: catalog.e, blocks_per_segment: catalog.blocks_per_segment()? })
    }

    /// Routes a read at block `number`. `local` says whether the node's own database can serve
    /// it; canonicality always comes from the node.
    pub fn route(&self, number: u64, local: bool) -> Route {
        if local {
            return Route::Local;
        }
        match self.h {
            Some(h) if number <= h && number >= self.e => Route::Archive,
            Some(h) if number <= h => Route::HistoryPruned,
            _ => Route::Unavailable,
        }
    }

    /// Returns `H` or an `Unavailable` error when nothing is published.
    pub fn require_h(&self) -> Result<u64, HistoryStoreError> {
        self.h.ok_or_else(|| HistoryStoreError::Unavailable {
            reason: "archive has no published segment".to_string(),
        })
    }

    /// Splits `from..=to` at the archive boundary into the part the archive serves (`E..=H`)
    /// and the rest, which must come from the node.
    pub fn split(&self, from: u64, to: u64) -> RangeSplit {
        let archive = self.h.and_then(|h| {
            let start = from.max(self.e);
            let end = to.min(h);
            (start <= end).then_some((start, end))
        });
        let local = match self.h {
            None => Some((from, to)),
            Some(h) if to > h => Some((from.max(h + 1), to)),
            Some(_) => None,
        };
        let pruned =
            (self.h.is_some() && from < self.e).then(|| (from, to.min(self.e - 1)));
        RangeSplit { archive, local, pruned }
    }
}

/// A block range split at the archive boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSplit {
    /// Sub-range served by the archive.
    pub archive: Option<(u64, u64)>,
    /// Sub-range above `H` that only the node can serve.
    pub local: Option<(u64, u64)>,
    /// Sub-range below `E`.
    pub pruned: Option<(u64, u64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each route is returned at the documented boundaries.
    #[test]
    fn routes_by_bounds() {
        let snapshot = ArchiveSnapshot { h: Some(15), e: 4, blocks_per_segment: 4 };
        assert_eq!(snapshot.route(2, true), Route::Local, "local wins");
        assert_eq!(snapshot.route(4, false), Route::Archive, "E is served");
        assert_eq!(snapshot.route(15, false), Route::Archive, "H is served");
        assert_eq!(snapshot.route(3, false), Route::HistoryPruned, "below E is pruned");
        assert_eq!(snapshot.route(16, false), Route::Unavailable, "above H is unavailable");
        let empty = ArchiveSnapshot { h: None, e: 0, blocks_per_segment: 4 };
        assert_eq!(empty.route(0, false), Route::Unavailable, "empty archive is unavailable");
    }

    /// Ranges split at E and H.
    #[test]
    fn splits_ranges_at_boundaries() {
        let snapshot = ArchiveSnapshot { h: Some(15), e: 4, blocks_per_segment: 4 };
        assert_eq!(
            snapshot.split(0, 20),
            RangeSplit { archive: Some((4, 15)), local: Some((16, 20)), pruned: Some((0, 3)) },
            "range spanning everything"
        );
        assert_eq!(
            snapshot.split(5, 9),
            RangeSplit { archive: Some((5, 9)), local: None, pruned: None },
            "range inside the archive"
        );
        assert_eq!(
            snapshot.split(17, 19),
            RangeSplit { archive: None, local: Some((17, 19)), pruned: None },
            "range above H"
        );
    }
}
