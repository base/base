//! redb-backed claim table and transactional at-most-once logic.
//!
//! The store keeps two tables in one redb database file (kept separate from the
//! canonical node MDBX store):
//!
//! * **claim table** — key `chain_id(8 BE) || victim_tx_hash(32)` = 40 bytes;
//!   value a fixed 41-byte record `version(1) || campaign_id(32) ||
//!   claimed_at_unix(8 BE)`. The uniqueness key is global-per-victim; the
//!   campaign id is provenance only.
//! * **metadata table** — a single `"meta"` row holding `version(1) ||
//!   creation_nonce(32)`, written once at bootstrap. The nonce is the
//!   [`StoreIdentity`] used to distinguish an attested store from a fresh empty
//!   one and to bind minted proofs to that store.
//!
//! All writes commit with [`Durability::Immediate`]; a `Claimed` proof is only
//! returned after `commit()` succeeds.

use alloy_primitives::B256;
use redb::{Database, Durability, ReadableTable, TableDefinition, TableError};

use super::types::{CampaignId, ClaimResult, ClaimStoreError, StoreIdentity, VictimClaim};

/// Fixed record format version. Any other value on read is fail-closed
/// [`ClaimStoreError::Corruption`] (unknown version is never trusted).
pub(super) const FORMAT_VERSION: u8 = 0x01;

/// Length of a claim table key: `chain_id(8) || victim_tx_hash(32)`.
const KEY_LEN: usize = 40;
/// Length of a claim table value: `version(1) || campaign(32) || ts(8)`.
const VALUE_LEN: usize = 41;
/// Length of the metadata value: `version(1) || creation_nonce(32)`.
const META_LEN: usize = 33;

/// Claim table: victim key -> fixed claim record.
pub(super) const CLAIM_TABLE: TableDefinition<'static, &[u8; KEY_LEN], &[u8; VALUE_LEN]> =
    TableDefinition::new("victim_claims");

/// Singleton metadata table: `"meta"` -> `version || creation_nonce`.
pub(super) const META_TABLE: TableDefinition<'static, &str, &[u8; META_LEN]> =
    TableDefinition::new("victim_claim_meta");

/// The single metadata key.
const META_KEY: &str = "meta";

/// Encodes the 40-byte claim key from `(chain_id, victim_tx_hash)`.
pub(super) fn encode_key(chain_id: u64, victim_tx_hash: B256) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    key[..8].copy_from_slice(&chain_id.to_be_bytes());
    key[8..].copy_from_slice(victim_tx_hash.as_slice());
    key
}

/// Encodes the fixed 41-byte claim value.
fn encode_value(campaign_id: CampaignId, claimed_at_unix: u64) -> [u8; VALUE_LEN] {
    let mut value = [0u8; VALUE_LEN];
    value[0] = FORMAT_VERSION;
    value[1..33].copy_from_slice(campaign_id.as_bytes());
    value[33..].copy_from_slice(&claimed_at_unix.to_be_bytes());
    value
}

/// Validates a stored claim record's version. Length is guaranteed by the redb
/// fixed-width value type; the version byte is checked fail-closed.
fn verify_claim_record(bytes: &[u8; VALUE_LEN]) -> Result<(), ClaimStoreError> {
    if bytes[0] != FORMAT_VERSION {
        return Err(ClaimStoreError::Corruption(format!(
            "unknown claim record version {:#x}",
            bytes[0]
        )));
    }
    Ok(())
}

/// Reads 32 CSPRNG bytes from the OS (`/dev/urandom`) for the creation nonce.
/// Provisioning-only; used solely by [`bootstrap_metadata`], so it is gated
/// behind the same feature and absent from the live-node build.
#[cfg(any(test, feature = "r9-provisioning"))]
fn generate_creation_nonce() -> Result<[u8; 32], ClaimStoreError> {
    use std::io::Read;
    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    let mut nonce = [0u8; 32];
    file.read_exact(&mut nonce).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    Ok(nonce)
}

/// Current unix time in seconds (provenance timestamp for the claim record).
fn now_unix() -> Result<u64, ClaimStoreError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| ClaimStoreError::Io(format!("system clock before unix epoch: {e}")))
}

/// Ensures the metadata row exists, creating it (with a fresh CSPRNG nonce) on
/// first bootstrap. Idempotent: on a store that already has metadata it adopts
/// and returns the existing identity (provisioning never overwrites a nonce).
/// Returns the store's [`StoreIdentity`]. Provisioning-only: gated behind the
/// `r9-provisioning` feature (plus cfg(test)) so it is not in the live build.
#[cfg(any(test, feature = "r9-provisioning"))]
pub(super) fn bootstrap_metadata(db: &Database) -> Result<StoreIdentity, ClaimStoreError> {
    // Fast path: metadata already present — adopt it without a write.
    if let Some(identity) = read_metadata(db)? {
        return Ok(identity);
    }

    let mut write = db.begin_write().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    write.set_durability(Durability::Immediate);
    let identity = {
        let mut table =
            write.open_table(META_TABLE).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        // Re-check inside the write txn in case of a concurrent bootstrap.
        if let Some(guard) = table.get(META_KEY).map_err(|e| ClaimStoreError::Io(e.to_string()))? {
            decode_metadata(guard.value())?
        } else {
            let nonce = generate_creation_nonce()?;
            let mut record = [0u8; META_LEN];
            record[0] = FORMAT_VERSION;
            record[1..].copy_from_slice(&nonce);
            table.insert(META_KEY, &record).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
            StoreIdentity::new(nonce)
        }
    };
    write.commit().map_err(|e| ClaimStoreError::CommitFailed(e.to_string()))?;
    Ok(identity)
}

/// Reads the metadata identity if present, `None` if the metadata table or row
/// is absent. Corruption (bad length/version) is fail-closed.
fn read_metadata(db: &Database) -> Result<Option<StoreIdentity>, ClaimStoreError> {
    let read = db.begin_read().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    let table = match read.open_table(META_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(ClaimStoreError::Io(e.to_string())),
    };
    let guard = match table.get(META_KEY).map_err(|e| ClaimStoreError::Io(e.to_string()))? {
        Some(guard) => guard,
        None => return Ok(None),
    };
    Ok(Some(decode_metadata(guard.value())?))
}

/// Decodes a metadata record, verifying the version byte fail-closed.
fn decode_metadata(bytes: &[u8; META_LEN]) -> Result<StoreIdentity, ClaimStoreError> {
    if bytes[0] != FORMAT_VERSION {
        return Err(ClaimStoreError::Corruption(format!(
            "unknown metadata version {:#x}",
            bytes[0]
        )));
    }
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&bytes[1..]);
    Ok(StoreIdentity::new(nonce))
}

/// Verifies an opened store's metadata matches the expected identity. A missing
/// table/row (lost or freshly-created empty store) or a nonce mismatch is
/// [`ClaimStoreError::StoreIdentityMismatch`] — never auto-recreated.
pub(super) fn verify_metadata(
    db: &Database,
    expected: StoreIdentity,
) -> Result<(), ClaimStoreError> {
    match read_metadata(db)? {
        Some(found) if found == expected => Ok(()),
        Some(_) | None => Err(ClaimStoreError::StoreIdentityMismatch),
    }
}

/// Executes the at-most-once claim transaction.
///
/// Order: `begin_write` (Immediate durability) -> `get(key)`; if present return
/// `AlreadyClaimed` (abort, no commit); if absent `insert` then `commit`. A
/// `Claimed` proof carrying `identity` is minted **only after** the commit
/// succeeds. Any redb error means the record is not durably committed / the
/// outcome is unknown, so no proof is issued.
pub(super) fn try_claim_txn(
    db: &Database,
    identity: StoreIdentity,
    chain_id: u64,
    victim_tx_hash: B256,
    campaign_id: CampaignId,
) -> Result<ClaimResult, ClaimStoreError> {
    let key = encode_key(chain_id, victim_tx_hash);
    let value = encode_value(campaign_id, now_unix()?);

    let mut write = db.begin_write().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    write.set_durability(Durability::Immediate);

    let inserted = {
        let mut table =
            write.open_table(CLAIM_TABLE).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        // Resolve the lookup (and validate any existing record fail-closed) in a
        // statement so the read guard's borrow is released before `insert`.
        let already =
            match table.get(&key).map_err(|e| ClaimStoreError::Corruption(e.to_string()))? {
                Some(guard) => {
                    verify_claim_record(guard.value())?;
                    true
                }
                None => false,
            };
        if already {
            false
        } else {
            table.insert(&key, &value).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
            true
        }
    };

    if !inserted {
        // Abort the write transaction (no durable change) and report the
        // existing global claim. Propagate any abort error so every redb
        // failure latches fail-closed rather than being silently swallowed.
        write.abort().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        return Ok(ClaimResult::AlreadyClaimed);
    }

    // Commit is the proof point: only a successful durable commit mints a claim.
    write.commit().map_err(|e| ClaimStoreError::CommitFailed(e.to_string()))?;
    Ok(ClaimResult::Claimed(VictimClaim::new_internal(
        chain_id,
        victim_tx_hash,
        campaign_id,
        identity,
    )))
}

/// Test-only: probe whether a victim is durably claimed, opening the database
/// read-only without acquiring the singleton lock or checking identity. Used by
/// crash-cut tests to assert an uncommitted partial write is not exposed.
#[cfg(test)]
pub(super) fn test_probe_claimed(
    db_path: &std::path::Path,
    chain_id: u64,
    victim_tx_hash: B256,
) -> Result<bool, ClaimStoreError> {
    let db = Database::open(db_path).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    let key = encode_key(chain_id, victim_tx_hash);
    let read = db.begin_read().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    let table = match read.open_table(CLAIM_TABLE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(ClaimStoreError::Io(e.to_string())),
    };
    let present = table.get(&key).map_err(|e| ClaimStoreError::Io(e.to_string()))?.is_some();
    Ok(present)
}

/// Test-only: write a raw claim record (arbitrary version byte) for a victim,
/// committing durably. Used to exercise the unknown-version fail-closed path.
#[cfg(test)]
pub(super) fn test_put_raw_claim(
    db: &Database,
    chain_id: u64,
    victim_tx_hash: B256,
    version: u8,
) -> Result<(), ClaimStoreError> {
    let key = encode_key(chain_id, victim_tx_hash);
    let mut record = encode_value(CampaignId::new([0u8; 32]), 0);
    record[0] = version;
    let mut write = db.begin_write().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    write.set_durability(Durability::Immediate);
    {
        let mut table =
            write.open_table(CLAIM_TABLE).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        table.insert(&key, &record).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    }
    write.commit().map_err(|e| ClaimStoreError::CommitFailed(e.to_string()))?;
    Ok(())
}

/// Test-only: write a raw metadata record (arbitrary version byte) into a fresh
/// database, committing durably. Used to exercise the unknown metadata-version
/// fail-closed path.
#[cfg(test)]
pub(super) fn test_put_raw_meta(
    db: &Database,
    version: u8,
    nonce: [u8; 32],
) -> Result<(), ClaimStoreError> {
    let mut record = [0u8; META_LEN];
    record[0] = version;
    record[1..].copy_from_slice(&nonce);
    let mut write = db.begin_write().map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    write.set_durability(Durability::Immediate);
    {
        let mut table =
            write.open_table(META_TABLE).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
        table.insert(META_KEY, &record).map_err(|e| ClaimStoreError::Io(e.to_string()))?;
    }
    write.commit().map_err(|e| ClaimStoreError::CommitFailed(e.to_string()))?;
    Ok(())
}

/// Test-only: bootstrap a store at `db_path`, begin a claim write, insert the
/// victim record, and **park without committing**. Simulates a crash mid-write:
/// the parent process SIGKILLs this process so the open (uncommitted) write is
/// lost. Never returns.
#[cfg(test)]
pub(super) fn test_insert_no_commit_and_park(
    db_path: &std::path::Path,
    chain_id: u64,
    victim_tx_hash: B256,
    campaign_id: CampaignId,
) -> ! {
    let db = Database::create(db_path).expect("create db");
    bootstrap_metadata(&db).expect("bootstrap metadata");
    let key = encode_key(chain_id, victim_tx_hash);
    let value = encode_value(campaign_id, now_unix().expect("clock"));
    let mut write = db.begin_write().expect("begin_write");
    write.set_durability(Durability::Immediate);
    {
        let mut table = write.open_table(CLAIM_TABLE).expect("open_table");
        table.insert(&key, &value).expect("insert");
    }
    // Deliberately do NOT commit. Signal readiness, then keep the write txn open
    // and park so the parent can hard-kill us before commit.
    let ready = db_path.with_extension("ready");
    std::fs::write(&ready, b"ready").expect("ready marker");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
