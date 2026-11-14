//! `catalog.json`, the only overwritten archive object, published by compare-and-swap
//! (TDD §2.4).

use std::{collections::BTreeMap, sync::Arc};

use alloy_primitives::B256;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

use crate::{ArchiveLayout, HistoryStoreError};

/// Archive format version this crate reads and writes.
pub const ARCHIVE_FORMAT: u32 = 1;

/// One run of segments sharing a size, starting at `from_block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentSpan {
    /// First block covered by this span.
    pub from_block: u64,
    /// Blocks per segment `N` in this span.
    pub blocks_per_segment: u64,
}

/// A segment retired by the pruner and waiting out its grace period before deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredSegment {
    /// Segment number.
    pub segment: u64,
    /// Unix time in milliseconds when `E` moved past the segment.
    pub retired_at_ms: u64,
}

/// Contents of `catalog.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Incremented by one on every successful update.
    pub generation: u64,
    /// Chain id the archive belongs to.
    pub chain_id: u64,
    /// Genesis block hash.
    pub genesis_hash: B256,
    /// Archive format version.
    pub format: u32,
    /// Segment size runs. This proof of concept supports exactly one run starting at block 0.
    #[serde(rename = "N")]
    pub n: Vec<SegmentSpan>,
    /// Highest block in a published segment; `None` before the first segment.
    #[serde(rename = "H")]
    pub h: Option<u64>,
    /// Earliest block retained.
    #[serde(rename = "E")]
    pub e: u64,
    /// Compression dictionaries by domain (`dict/<domain>-v<n>`); empty in this proof of concept.
    pub dictionaries: BTreeMap<String, String>,
    /// Index root, relative to the archive prefix.
    pub index: String,
    /// Segments below `E` whose objects are kept until their grace period ends. Entries are
    /// removed once swept, so the list holds at most the segments retired within one grace
    /// period.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<RetiredSegment>,
    /// Ownership token: the epoch of the writer, pruner or GC instance allowed to change the
    /// archive. It is the instance's `SlateDB` writer epoch, claimed by CAS after the index is
    /// opened and only if greater than the stored value, so one number fences an older
    /// instance's catalog updates, index writes and deletions together.
    #[serde(default)]
    pub writer_epoch: u64,
}

impl Catalog {
    /// Creates the generation-0 catalog of an empty archive.
    pub fn new(chain_id: u64, genesis_hash: B256, blocks_per_segment: u64) -> Self {
        Self {
            generation: 0,
            chain_id,
            genesis_hash,
            format: ARCHIVE_FORMAT,
            n: vec![SegmentSpan { from_block: 0, blocks_per_segment }],
            h: None,
            e: 0,
            dictionaries: BTreeMap::new(),
            index: format!("{}/", ArchiveLayout::INDEX),
            retired: Vec::new(),
            writer_epoch: 0,
        }
    }

    /// Returns the segment size `N`, rejecting catalogs with more than one span.
    pub fn blocks_per_segment(&self) -> Result<u64, HistoryStoreError> {
        match self.n.as_slice() {
            [SegmentSpan { from_block: 0, blocks_per_segment }] if *blocks_per_segment > 0 => {
                Ok(*blocks_per_segment)
            }
            _ => Err(HistoryStoreError::Catalog {
                reason: format!("unsupported segment spans {:?}", self.n),
            }),
        }
    }

    /// Returns the next segment the writer must publish: `(H + 1) / N`.
    pub fn next_segment(&self) -> Result<u64, HistoryStoreError> {
        let n = self.blocks_per_segment()?;
        Ok(self.h.map_or(0, |h| (h + 1) / n))
    }

    /// Returns whether block `number` lies in a published, retained segment (`E ≤ n ≤ H`).
    pub fn contains(&self, number: u64) -> bool {
        self.h.is_some_and(|h| number <= h) && number >= self.e
    }

    /// Checks the catalog belongs to the expected chain and format.
    pub fn verify(&self, chain_id: u64, genesis_hash: B256) -> Result<(), HistoryStoreError> {
        if self.chain_id != chain_id {
            return Err(HistoryStoreError::CatalogMismatch {
                field: "chain_id",
                expected: chain_id.to_string(),
                actual: self.chain_id.to_string(),
            });
        }
        if self.genesis_hash != genesis_hash {
            return Err(HistoryStoreError::CatalogMismatch {
                field: "genesis_hash",
                expected: genesis_hash.to_string(),
                actual: self.genesis_hash.to_string(),
            });
        }
        if self.format != ARCHIVE_FORMAT {
            return Err(HistoryStoreError::CatalogMismatch {
                field: "format",
                expected: ARCHIVE_FORMAT.to_string(),
                actual: self.format.to_string(),
            });
        }
        self.blocks_per_segment().map(|_| ())
    }

    /// Fails with [`HistoryStoreError::Fenced`] unless `owned` is the catalog's writer epoch.
    pub const fn check_owner(&self, owned: u64) -> Result<(), HistoryStoreError> {
        if self.writer_epoch != owned {
            return Err(HistoryStoreError::Fenced { owned, current: self.writer_epoch });
        }
        Ok(())
    }

    /// Checks that `next` is a legal successor: generation + 1, and `H`, `E` never decrease.
    pub fn check_successor(&self, next: &Self) -> Result<(), HistoryStoreError> {
        if next.generation != self.generation + 1 {
            return Err(HistoryStoreError::Regression {
                reason: format!("generation {} after {}", next.generation, self.generation),
            });
        }
        if next.h < self.h {
            return Err(HistoryStoreError::Regression {
                reason: format!("H {:?} after {:?}", next.h, self.h),
            });
        }
        if next.writer_epoch < self.writer_epoch {
            return Err(HistoryStoreError::Regression {
                reason: format!("writer epoch {} after {}", next.writer_epoch, self.writer_epoch),
            });
        }
        if next.e < self.e {
            return Err(HistoryStoreError::Regression {
                reason: format!("E {} after {}", next.e, self.e),
            });
        }
        if (next.chain_id, next.genesis_hash, next.format, &next.n)
            != (self.chain_id, self.genesis_hash, self.format, &self.n)
        {
            return Err(HistoryStoreError::Regression {
                reason: "chain identity or segment size changed".to_string(),
            });
        }
        Ok(())
    }
}

/// A catalog together with the object version it was read or written at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedCatalog {
    /// Catalog contents.
    pub catalog: Catalog,
    /// Version to pass as the `If-Match` precondition of the next update.
    pub version: UpdateVersion,
}

/// Reads and conditionally writes `catalog.json`.
#[derive(Debug, Clone)]
pub struct CatalogStore {
    /// Bucket holding the archive.
    pub store: Arc<dyn ObjectStore>,
    /// Archive layout.
    pub layout: ArchiveLayout,
}

impl CatalogStore {
    /// Creates a catalog store.
    pub const fn new(store: Arc<dyn ObjectStore>, layout: ArchiveLayout) -> Self {
        Self { store, layout }
    }

    /// Reads the catalog, returning `None` when the archive has not been initialised.
    pub async fn load(&self) -> Result<Option<VersionedCatalog>, HistoryStoreError> {
        let result = match self.store.get(&self.layout.catalog()).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        let bytes = result.bytes().await?;
        let catalog = serde_json::from_slice(&bytes)
            .map_err(|err| HistoryStoreError::Catalog { reason: err.to_string() })?;
        Ok(Some(VersionedCatalog { catalog, version }))
    }

    /// Reads the catalog and fails if the archive has not been initialised.
    pub async fn load_required(&self) -> Result<VersionedCatalog, HistoryStoreError> {
        self.load().await?.ok_or_else(|| HistoryStoreError::Catalog {
            reason: "catalog.json missing".to_string(),
        })
    }

    /// Creates the catalog if absent (`If-None-Match: *`). If one exists it is returned instead,
    /// after checking it belongs to the same chain.
    pub async fn init(&self, catalog: &Catalog) -> Result<VersionedCatalog, HistoryStoreError> {
        let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
        match self.store.put_opts(&self.layout.catalog(), Self::payload(catalog)?, opts).await {
            Ok(result) => Ok(VersionedCatalog { catalog: catalog.clone(), version: result.into() }),
            Err(object_store::Error::AlreadyExists { .. }) => {
                let existing = self.load_required().await?;
                existing.catalog.verify(catalog.chain_id, catalog.genesis_hash)?;
                if existing.catalog.n != catalog.n {
                    return Err(HistoryStoreError::CatalogMismatch {
                        field: "N",
                        expected: format!("{:?}", catalog.n),
                        actual: format!("{:?}", existing.catalog.n),
                    });
                }
                Ok(existing)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Replaces the catalog if it is still at `current.version` (`If-Match`).
    ///
    /// Returns [`HistoryStoreError::CasConflict`] when another writer updated it first and
    /// [`HistoryStoreError::BackendUnsupported`] on backends without conditional updates.
    pub async fn update(
        &self,
        current: &VersionedCatalog,
        next: Catalog,
    ) -> Result<VersionedCatalog, HistoryStoreError> {
        current.catalog.check_successor(&next)?;
        let opts =
            PutOptions { mode: PutMode::Update(current.version.clone()), ..Default::default() };
        match self.store.put_opts(&self.layout.catalog(), Self::payload(&next)?, opts).await {
            Ok(result) => Ok(VersionedCatalog { catalog: next, version: result.into() }),
            Err(object_store::Error::Precondition { .. }) => Err(HistoryStoreError::CasConflict),
            Err(object_store::Error::NotImplemented { .. }) => {
                Err(HistoryStoreError::BackendUnsupported)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Updates the catalog on behalf of the instance holding writer epoch `owned`: fails with
    /// [`HistoryStoreError::Fenced`] if a newer instance claimed the archive since `current`
    /// was read. Because the CAS is conditional on `current`'s version, a claim that lands
    /// between the check and the write makes the write fail with `CasConflict`, and the
    /// caller's reread then reports `Fenced`.
    pub async fn update_owned(
        &self,
        current: &VersionedCatalog,
        next: Catalog,
        owned: u64,
    ) -> Result<VersionedCatalog, HistoryStoreError> {
        current.catalog.check_owner(owned)?;
        self.update(current, next).await
    }

    /// Claims ownership of the archive under `epoch`, the caller's `SlateDB` writer epoch: CAS
    /// `writer_epoch := epoch` if `epoch` is greater than the stored epoch, and return the catalog
    /// as written. Fails with [`HistoryStoreError::Fenced`] when an instance with an equal or
    /// newer epoch already claimed, because that instance has also fenced this one's index.
    pub async fn claim_at(&self, epoch: u64) -> Result<VersionedCatalog, HistoryStoreError> {
        loop {
            let current = self.load_required().await?;
            if current.catalog.writer_epoch >= epoch {
                return Err(HistoryStoreError::Fenced {
                    owned: epoch,
                    current: current.catalog.writer_epoch,
                });
            }
            let next = Catalog {
                generation: current.catalog.generation + 1,
                writer_epoch: epoch,
                ..current.catalog.clone()
            };
            match self.update(&current, next).await {
                Ok(claimed) => return Ok(claimed),
                Err(HistoryStoreError::CasConflict) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Serialises a catalog.
    pub fn payload(catalog: &Catalog) -> Result<PutPayload, HistoryStoreError> {
        let bytes = serde_json::to_vec_pretty(catalog)
            .map_err(|err| HistoryStoreError::Catalog { reason: err.to_string() })?;
        Ok(PutPayload::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use object_store::{local::LocalFileSystem, memory::InMemory};

    use super::*;

    /// Returns a catalog store over a fresh in-memory bucket.
    fn memory_store() -> CatalogStore {
        CatalogStore::new(Arc::new(InMemory::new()), ArchiveLayout::new("chain"))
    }

    /// Returns `current` advanced by one generation with a new `H`.
    fn advanced(current: &Catalog, h: u64) -> Catalog {
        Catalog { generation: current.generation + 1, h: Some(h), ..current.clone() }
    }

    /// A claim fences every earlier owner: its owned update fails with `Fenced` and leaves the
    /// catalog unchanged, while the new owner's update succeeds. A claim at an epoch that is not
    /// newer than the stored one is itself fenced.
    #[tokio::test]
    async fn claim_fences_earlier_owner() {
        let store = memory_store();
        let catalog = Catalog::new(10, B256::with_last_byte(1), 4);
        store.init(&catalog).await.unwrap();
        let old = store.claim_at(1).await.unwrap().catalog.writer_epoch;
        let new = store.claim_at(2).await.unwrap().catalog.writer_epoch;
        assert_eq!((old, new), (1, 2), "claims record the caller's epoch");
        let stale = store.claim_at(1).await.unwrap_err();
        assert!(
            matches!(stale, HistoryStoreError::Fenced { owned: 1, current: 2 }),
            "a claim below the stored epoch is fenced, got {stale}"
        );
        let current = store.load_required().await.unwrap();
        let err =
            store.update_owned(&current, advanced(&current.catalog, 3), old).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Fenced { owned: 1, current: 2 }),
            "the earlier owner is fenced, got {err}"
        );
        assert_eq!(store.load_required().await.unwrap(), current, "fenced update changes nothing");
        let updated =
            store.update_owned(&current, advanced(&current.catalog, 3), new).await.unwrap();
        assert_eq!(updated.catalog.h, Some(3), "the current owner updates");
        let regress = Catalog { writer_epoch: 1, ..advanced(&updated.catalog, 7) };
        assert!(
            matches!(
                store.update(&updated, regress).await,
                Err(HistoryStoreError::Regression { .. })
            ),
            "writer epoch never decreases"
        );
    }

    /// `init` creates once and returns the existing catalog afterwards.
    #[tokio::test]
    async fn init_is_create_if_absent() {
        let store = memory_store();
        let catalog = Catalog::new(10, B256::with_last_byte(1), 4);
        let first = store.init(&catalog).await.unwrap();
        let updated = store.update(&first, advanced(&catalog, 3)).await.unwrap();
        let second = store.init(&catalog).await.unwrap();
        assert_eq!(second.catalog, updated.catalog, "init must not overwrite an existing catalog");
        let other_chain = Catalog::new(11, B256::with_last_byte(1), 4);
        assert_eq!(
            store.init(&other_chain).await.unwrap_err().to_string(),
            "catalog chain_id mismatch: expected 11, got 10",
            "init on a foreign archive is rejected"
        );
    }

    /// A stale version loses with `CasConflict`; reloading and retrying wins.
    #[tokio::test]
    async fn stale_version_conflicts_then_retry_wins() {
        let store = memory_store();
        let catalog = Catalog::new(10, B256::ZERO, 4);
        let v0 = store.init(&catalog).await.unwrap();
        store.update(&v0, advanced(&catalog, 3)).await.unwrap();
        let err = store.update(&v0, advanced(&catalog, 7)).await.unwrap_err();
        assert!(matches!(err, HistoryStoreError::CasConflict), "stale etag must conflict: {err}");
        let v1 = store.load_required().await.unwrap();
        let v2 = store.update(&v1, advanced(&v1.catalog, 7)).await.unwrap();
        assert_eq!(v2.catalog.generation, 2, "retry lands generation 2");
        assert_eq!(v2.catalog.h, Some(7), "retry publishes H=7");
    }

    /// Two concurrent updates from the same version: exactly one wins.
    #[tokio::test]
    async fn concurrent_cas_has_one_winner() {
        let store = memory_store();
        let catalog = Catalog::new(10, B256::ZERO, 4);
        let v0 = store.init(&catalog).await.unwrap();
        let (a, b) = tokio::join!(
            store.update(&v0, advanced(&catalog, 3)),
            store.update(&v0, advanced(&catalog, 3))
        );
        let wins = [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count();
        let conflicts =
            [&a, &b].iter().filter(|r| matches!(r, Err(HistoryStoreError::CasConflict))).count();
        assert_eq!(wins, 1, "exactly one CAS must win");
        assert_eq!(conflicts, 1, "exactly one CAS must conflict");
        assert_eq!(store.load_required().await.unwrap().catalog.generation, 1, "generation +1");
    }

    /// H and E never decrease and the generation always advances by one.
    #[tokio::test]
    async fn regressions_are_rejected() {
        let store = memory_store();
        let catalog = Catalog::new(10, B256::ZERO, 4);
        let v0 = store.init(&catalog).await.unwrap();
        let v1 = store.update(&v0, advanced(&catalog, 7)).await.unwrap();
        let lower_h = Catalog { generation: 2, h: Some(3), ..v1.catalog.clone() };
        assert_eq!(
            store.update(&v1, lower_h).await.unwrap_err().to_string(),
            "catalog regression: H Some(3) after Some(7)",
            "H must not decrease"
        );
        let v2 = store.update(&v1, Catalog { generation: 2, e: 4, ..v1.catalog.clone() }).await;
        let v2 = v2.unwrap();
        let lower_e = Catalog { generation: 3, e: 0, ..v2.catalog.clone() };
        assert_eq!(
            store.update(&v2, lower_e).await.unwrap_err().to_string(),
            "catalog regression: E 0 after 4",
            "E must not decrease"
        );
        let skip = Catalog { generation: 5, ..v2.catalog.clone() };
        assert_eq!(
            store.update(&v2, skip).await.unwrap_err().to_string(),
            "catalog regression: generation 5 after 2",
            "generation must advance by exactly one"
        );
    }

    /// Chain id, genesis and format mismatches are rejected with the field name.
    #[test]
    fn verify_rejects_foreign_catalogs() {
        let catalog = Catalog::new(10, B256::with_last_byte(1), 4);
        assert!(catalog.verify(10, B256::with_last_byte(1)).is_ok(), "matching catalog verifies");
        assert_eq!(
            catalog.verify(10, B256::with_last_byte(2)).unwrap_err().to_string(),
            format!(
                "catalog genesis_hash mismatch: expected {}, got {}",
                B256::with_last_byte(2),
                B256::with_last_byte(1)
            ),
            "genesis mismatch is rejected"
        );
        let future = Catalog { format: 2, ..catalog };
        assert_eq!(
            future.verify(10, B256::with_last_byte(1)).unwrap_err().to_string(),
            "catalog format mismatch: expected 1, got 2",
            "unknown format is rejected"
        );
    }

    /// `LocalFileSystem` has no conditional update, which surfaces as a typed error.
    #[tokio::test]
    async fn local_filesystem_cas_is_backend_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::new(
            Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap()),
            ArchiveLayout::new(""),
        );
        let catalog = Catalog::new(10, B256::ZERO, 4);
        let v0 = store.init(&catalog).await.unwrap();
        let err = store.update(&v0, advanced(&catalog, 3)).await.unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::BackendUnsupported),
            "LocalFileSystem update must be BackendUnsupported, got {err}"
        );
    }

    /// The catalog JSON uses the TDD's field names.
    #[test]
    fn json_field_names_match_tdd() {
        let json = serde_json::to_value(Catalog::new(10, B256::ZERO, 8)).unwrap();
        let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "E",
                "H",
                "N",
                "chain_id",
                "dictionaries",
                "format",
                "generation",
                "genesis_hash",
                "index",
                "writer_epoch"
            ],
            "catalog.json fields"
        );
    }
}
