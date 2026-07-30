//! Submit-suppression freshness sources: a monotonic-epoch JSON flag file (written
//! out-of-band by the TS `p2-integration-orchestrator`) plus a dedicated redb
//! high-water anchor that rejects an epoch rollback.
//!
//! Both are fail-closed: any parse error, version mismatch, missing/empty/corrupt
//! store, or non-monotonic epoch yields no [`super::proofs::SubmitSuppressionClear`].
//!
//! The high-water store is a SEPARATE redb file from the R9 claim store (its own
//! compile-pinned path) and is arm-owned; R9 is unchanged.

use redb::{Database, Durability, ReadableTable, TableDefinition};

/// Compile-pinned absolute path of the suppression flag file (written by the TS
/// orchestrator with an atomic, strictly-monotonic epoch).
pub(crate) const SUPPRESSION_FILE_PATH: &str = "/home/ubuntu/.config/mev-suppression.json";

/// Compile-pinned absolute path of the arm-owned high-water anchor DB (redb).
pub(crate) const SUPPRESSION_HW_DB_PATH: &str = "/home/ubuntu/.config/mev-suppression-hw.redb";

/// The single-row high-water table: `"hw"` -> highest observed epoch.
const HW_TABLE: TableDefinition<'static, &str, u64> = TableDefinition::new("suppression_hw");

/// The single high-water key.
const HW_KEY: &str = "hw";

/// A rollback / persistence failure of the high-water anchor. Every variant is
/// fail-closed: no proof is minted.
#[derive(Debug)]
pub enum SuppressionRollbackError {
    /// The observed epoch is strictly below the recorded high-water mark — a
    /// rollback. Fail-closed.
    Rollback {
        /// The epoch that was observed.
        observed: u64,
        /// The recorded high-water mark it violated.
        high_water: u64,
    },
    /// A redb I/O / commit failure while recording the high-water mark.
    Io(String),
}

impl core::fmt::Display for SuppressionRollbackError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Rollback { observed, high_water } => write!(
                formatter,
                "suppression epoch rollback: observed {observed} < high-water {high_water}"
            ),
            Self::Io(message) => write!(formatter, "suppression high-water io: {message}"),
        }
    }
}

impl core::error::Error for SuppressionRollbackError {}

/// The parsed, version-checked suppression file record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SuppressionFileRecord {
    /// Monotonic epoch stamped by the writer.
    pub epoch: u64,
    /// Whether submission is currently suppressed.
    pub suppressed: bool,
}

/// Stable checked suppression-file read class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuppressionFileReadError {
    /// File or writer state was unavailable.
    Unavailable,
    /// Canonical JSON fields were malformed.
    Invalid,
}

/// Fresh reader over the suppression flag file.
#[derive(Debug, Clone)]
pub(crate) struct SuppressionFileStore {
    path: std::path::PathBuf,
}

impl SuppressionFileStore {
    /// PRIVATE explicit-path constructor. Production may ONLY open the compile-pinned
    /// path via [`at_pinned_path`](Self::at_pinned_path); an arbitrary path is
    /// reachable only through the `#[cfg(test)]` [`new`](Self::new) seam.
    const fn at_path(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Opens the reader at the compile-pinned activation path (the ONLY production
    /// constructor).
    pub(crate) fn at_pinned_path() -> Self {
        Self::at_path(std::path::PathBuf::from(SUPPRESSION_FILE_PATH))
    }

    /// Test-only: open the reader at an arbitrary path (temp fixtures). `#[cfg(test)]`
    /// so production/arm-wiring code can NEVER point the suppression store at a
    /// caller-chosen path.
    #[cfg(test)]
    pub(crate) fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self::at_path(path.into())
    }

    /// Whether the writer's `O_EXCL` lock file (`<path>.lock`) is present. A present
    /// lock means the TS writer is mid-write OR crashed leaving a stale lock (the
    /// writer takes an `O_EXCL` lock with NO auto-steal). Either way the arm must
    /// fail-closed (treat as suppressed) so a partially-written or stale "clear"
    /// state can never authorize a submission. Uses `symlink_metadata` so even a
    /// broken-symlink lock entry counts as present.
    pub(crate) fn lock_present(&self) -> bool {
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        std::fs::symlink_metadata(std::path::PathBuf::from(lock)).is_ok()
    }

    /// Reads and parses the file fresh. Returns `None` fail-closed on any
    /// absence/read/parse error or a `version != 1`. Manual field extraction (no
    /// serde derive dependency) via `serde_json::Value`.
    pub(crate) fn read_fresh(&self) -> Option<SuppressionFileRecord> {
        let bytes = std::fs::read(&self.path).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let object = value.as_object()?;
        // version must be exactly 1.
        if object.get("version")?.as_u64()? != 1 {
            return None;
        }
        let epoch = object.get("epoch")?.as_u64()?;
        let suppressed = object.get("suppressed")?.as_bool()?;
        Some(SuppressionFileRecord { epoch, suppressed })
    }

    /// Reads the guarded canonical record while preserving unavailable versus invalid.
    pub(crate) fn read_fresh_guarded_checked(
        &self,
    ) -> Result<SuppressionFileRecord, SuppressionFileReadError> {
        if self.lock_present() {
            return Err(SuppressionFileReadError::Unavailable);
        }
        let bytes =
            std::fs::read(&self.path).map_err(|_| SuppressionFileReadError::Unavailable)?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SuppressionFileReadError::Invalid)?;
        let object = value.as_object().ok_or(SuppressionFileReadError::Invalid)?;
        if object.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
            return Err(SuppressionFileReadError::Invalid);
        }
        let epoch = object
            .get("epoch")
            .and_then(serde_json::Value::as_u64)
            .ok_or(SuppressionFileReadError::Invalid)?;
        let suppressed = object
            .get("suppressed")
            .and_then(serde_json::Value::as_bool)
            .ok_or(SuppressionFileReadError::Invalid)?;
        if self.lock_present() {
            return Err(SuppressionFileReadError::Unavailable);
        }
        Ok(SuppressionFileRecord { epoch, suppressed })
    }

    /// Lock-guarded fresh read (the SINGLE authoritative read path used by both the
    /// initial proof creation AND the egress re-validation). Fail-closed to `None`
    /// if the writer lock is present BEFORE the read, if the read/parse fails, OR if
    /// the lock appeared DURING the read (mid-write race). The double lock check
    /// closes the window where a stale `suppressed:false` is observed while the
    /// writer is mid-flight.
    pub(crate) fn read_fresh_guarded(&self) -> Option<SuppressionFileRecord> {
        if self.lock_present() {
            return None;
        }
        let record = self.read_fresh()?;
        if self.lock_present() {
            return None;
        }
        Some(record)
    }
}

/// The arm-owned redb high-water anchor. `observe(epoch)` records a strictly
/// non-decreasing high-water mark; an epoch below the mark is a fail-closed
/// [`SuppressionRollbackError::Rollback`].
#[derive(Debug)]
pub(crate) struct SuppressionEpochStore {
    db: Database,
}

impl SuppressionEpochStore {
    /// Provisions (creates if absent) the high-water DB. Provisioning-only: gated
    /// behind `arm-provisioning` (plus `cfg(test)`), so the activation node build
    /// never auto-creates a fresh (hw=0) anchor that would defeat rollback
    /// detection.
    #[cfg(any(test, feature = "arm-provisioning"))]
    pub(crate) fn bootstrap(path: impl AsRef<std::path::Path>) -> Result<Self, SuppressionRollbackError> {
        // `Database::create` also OPENS an existing valid DB, so bootstrap MUST be
        // IDEMPOTENT: it seeds `hw -> 0` only when the key is ABSENT, and PRESERVES
        // an existing high-water mark (re-running the provisioning build must never
        // reset e.g. hw=10 to 0 and void the rollback anchor — mirrors R9's
        // identity-preserving bootstrap).
        let db = Database::create(path.as_ref())
            .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
        let mut write =
            db.begin_write().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write
                .open_table(HW_TABLE)
                .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
            let present = table
                .get(HW_KEY)
                .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?
                .is_some();
            if !present {
                table
                    .insert(HW_KEY, 0u64)
                    .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
            }
        }
        write.commit().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
        Ok(Self { db })
    }

    /// Opens an already-provisioned anchor (never creates). A missing/empty/corrupt
    /// DB — OR a valid redb file whose `hw` table/key is absent — is fail-closed:
    /// auto-creation (and silently adopting a missing key as hw=0) is forbidden
    /// because it would disarm the rollback guard.
    /// Test-only: open an anchor at an arbitrary path. `#[cfg(test)]` so production
    /// may open ONLY the compile-pinned anchor via [`open_pinned`](Self::open_pinned).
    #[cfg(test)]
    pub(crate) fn open_existing(
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, SuppressionRollbackError> {
        Self::open_at(path.as_ref())
    }

    /// PRIVATE explicit-path opener (the real logic).
    fn open_at(path: &std::path::Path) -> Result<Self, SuppressionRollbackError> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > 0 => {}
            _ => {
                return Err(SuppressionRollbackError::Io(
                    "suppression high-water store missing or empty".to_string(),
                ));
            }
        }
        let db = Database::open(path)
            .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
        // Verify the anchor table AND the `hw` key exist (fail-closed if absent).
        {
            let read =
                db.begin_read().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
            let table = read.open_table(HW_TABLE).map_err(|_| {
                SuppressionRollbackError::Io("suppression high-water table absent".to_string())
            })?;
            let present = table
                .get(HW_KEY)
                .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?
                .is_some();
            if !present {
                return Err(SuppressionRollbackError::Io(
                    "suppression high-water key absent".to_string(),
                ));
            }
        }
        Ok(Self { db })
    }

    /// Opens the anchor at the compile-pinned activation path (the ONLY production
    /// opener).
    pub(crate) fn open_pinned() -> Result<Self, SuppressionRollbackError> {
        Self::open_at(std::path::Path::new(SUPPRESSION_HW_DB_PATH))
    }

    /// Records `epoch` as the new high-water mark iff it is not below the current
    /// mark. An epoch below the mark is a fail-closed rollback; a persistence
    /// failure is fail-closed I/O. Success means the mark is durably `>= epoch`.
    pub(crate) fn observe(&self, epoch: u64) -> Result<(), SuppressionRollbackError> {
        let mut write =
            self.db.begin_write().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
        write.set_durability(Durability::Immediate);
        // Resolve the decision with the table borrow fully released before deciding
        // to commit or abort `write` (mirrors the R9 claim-store pattern).
        let decision = {
            let mut table = write
                .open_table(HW_TABLE)
                .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
            let current = table
                .get(HW_KEY)
                .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?
                .map_or(0, |guard| guard.value());
            if epoch < current {
                Err(SuppressionRollbackError::Rollback { observed: epoch, high_water: current })
            } else {
                if epoch > current {
                    table
                        .insert(HW_KEY, epoch)
                        .map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
                }
                Ok(())
            }
        };
        match decision {
            Ok(()) => {
                write.commit().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
                Ok(())
            }
            Err(rollback) => {
                write.abort().map_err(|error| SuppressionRollbackError::Io(error.to_string()))?;
                Err(rollback)
            }
        }
    }

    /// Test-only view of the current high-water mark (`None` if unset).
    #[cfg(test)]
    pub(crate) fn debug_high_water(&self) -> Option<u64> {
        let read = self.db.begin_read().ok()?;
        let table = match read.open_table(HW_TABLE) {
            Ok(table) => table,
            Err(_) => return None,
        };
        table.get(HW_KEY).ok()?.map(|guard| guard.value())
    }
}

/// Provisioning-only curated entry point: bootstrap the compile-pinned suppression
/// high-water anchor (create-if-absent, idempotent — preserves an existing mark).
/// Gated behind `arm-provisioning` (a dedicated provisioning binary), so the
/// activation build never compiles a store creator. It is the ONLY public
/// provisioning surface; `SuppressionEpochStore` itself stays crate-private.
#[cfg(feature = "arm-provisioning")]
pub fn provision_suppression_anchor() -> Result<(), SuppressionRollbackError> {
    SuppressionEpochStore::bootstrap(SUPPRESSION_HW_DB_PATH)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::testkit as tk;

    #[test]
    fn bootstrap_observe_and_restart() {
        let dir = tk::TempDir::new("hw-restart");
        let path = dir.path.join("hw.redb");
        {
            let store = SuppressionEpochStore::bootstrap(&path).expect("bootstrap");
            store.observe(1).expect("observe 1");
            store.observe(4).expect("observe 4");
            assert_eq!(store.debug_high_water(), Some(4));
        }
        // Restart: open_existing must see the persisted high-water mark.
        let reopened = SuppressionEpochStore::open_existing(&path).expect("reopen");
        assert_eq!(reopened.debug_high_water(), Some(4));
        // Equal epoch is allowed (monotonic non-decreasing); lower is a rollback.
        reopened.observe(4).expect("observe equal");
        assert!(matches!(reopened.observe(2), Err(SuppressionRollbackError::Rollback { .. })));
        // High-water unchanged after a rejected rollback.
        assert_eq!(reopened.debug_high_water(), Some(4));
    }

    #[test]
    fn open_existing_missing_is_fail_closed() {
        let dir = tk::TempDir::new("hw-missing");
        let err = SuppressionEpochStore::open_existing(dir.path.join("absent.redb")).unwrap_err();
        assert!(matches!(err, SuppressionRollbackError::Io(_)));
    }

    #[test]
    fn bootstrap_existing_preserves_high_water() {
        // Re-running the provisioning bootstrap against an existing store must
        // PRESERVE the high-water mark, never reset it to 0 (rollback-anchor safety).
        let dir = tk::TempDir::new("hw-idem");
        let path = dir.path.join("hw.redb");
        {
            let store = SuppressionEpochStore::bootstrap(&path).expect("bootstrap");
            store.observe(10).expect("observe 10");
            assert_eq!(store.debug_high_water(), Some(10));
        }
        // Second bootstrap (drop-then-recreate) must keep hw=10, not reset to 0.
        let store = SuppressionEpochStore::bootstrap(&path).expect("re-bootstrap");
        assert_eq!(store.debug_high_water(), Some(10));
        // And a rollback below the preserved mark is still rejected.
        assert!(matches!(store.observe(3), Err(SuppressionRollbackError::Rollback { .. })));
    }

    #[test]
    fn bootstrap_writes_zero_anchor() {
        let dir = tk::TempDir::new("hw-zero");
        let store = SuppressionEpochStore::bootstrap(dir.path.join("hw.redb")).expect("bootstrap");
        // The `hw -> 0` key exists immediately after bootstrap.
        assert_eq!(store.debug_high_water(), Some(0));
    }

    #[test]
    fn open_existing_absent_hw_key_is_fail_closed() {
        // A VALID redb file whose `hw` table/key is absent must be fail-closed —
        // never silently adopted as hw=0 (which would void the rollback guard).
        let dir = tk::TempDir::new("hw-nokey");
        let path = dir.path.join("other.redb");
        {
            // Create a redb DB with a DIFFERENT table (no `hw`).
            let db = redb::Database::create(&path).expect("create");
            let other: redb::TableDefinition<'static, &str, u64> =
                redb::TableDefinition::new("not_hw");
            let write = db.begin_write().expect("write");
            {
                write.open_table(other).expect("open other");
            }
            write.commit().expect("commit");
        }
        let err = SuppressionEpochStore::open_existing(&path).unwrap_err();
        assert!(matches!(err, SuppressionRollbackError::Io(_)));
    }

    #[test]
    fn single_writer_fencing() {
        let dir = tk::TempDir::new("hw-fence");
        let path = dir.path.join("hw.redb");
        let _held = SuppressionEpochStore::bootstrap(&path).expect("bootstrap");
        // A second opener of the same inode is refused while the first is held.
        assert!(SuppressionEpochStore::open_existing(&path).is_err());
    }

    #[test]
    fn monotonic_advances_only_upward() {
        let dir = tk::TempDir::new("hw-mono");
        let store = SuppressionEpochStore::bootstrap(dir.path.join("hw.redb")).expect("bootstrap");
        store.observe(10).expect("10");
        // A lower epoch after a higher one is a rollback.
        assert!(store.observe(9).is_err());
        assert_eq!(store.debug_high_water(), Some(10));
        store.observe(11).expect("11");
        assert_eq!(store.debug_high_water(), Some(11));
    }
}
