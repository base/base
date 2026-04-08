//! The redb-backed safe head database.
//!
//! # Persistence and crash safety
//!
//! [redb] provides ACID guarantees for single-process access: each write
//! transaction is fully atomic on [`commit`]. An unclean shutdown (crash, kill
//! signal) in the middle of a write transaction leaves the database in its
//! pre-transaction state — no corruption, no partial writes. No manual recovery
//! is required after a restart.
//!
//! **Multi-process safety**: redb does not provide cross-process file locking.
//! Only one node instance must open a given database file at a time. Running two
//! node processes against the same `safedb` path results in undefined behaviour
//! and likely data corruption.
//!
//! # Encoding
//!
//! The table key is the **L1 block number** (`u64`). The value is a 72-byte
//! packed record:
//!
//! ```text
//! [ L1 hash (32) | L2 hash (32) | L2 number BE (8) ]
//! ```
//!
//! L1 hash is stored alongside the L1 number (its natural key) to avoid a
//! secondary L1 lookup in the RPC read path.
//!
//! [`commit`]: redb::WriteTransaction::commit

use std::{path::Path, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use async_trait::async_trait;
use base_protocol::{BlockInfo, L2BlockInfo};
use redb::{Database, ReadableTable, TableDefinition};

use crate::{SafeDBError, SafeDBReader, SafeHeadListener, SafeHeadResponse};

/// Table mapping L1 block number to (L1 hash || L2 hash || L2 number).
const SAFE_HEADS: TableDefinition<'_, u64, &[u8; 72]> = TableDefinition::new("safe_heads");

/// A persistent safe head database backed by [redb].
#[derive(Debug, Clone)]
pub struct SafeDB {
    /// The underlying redb database.
    db: Arc<Database>,
}

impl SafeDB {
    /// Opens (or creates) the safe head database at the given path.
    ///
    /// If the parent directory does not exist it is created automatically.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SafeDBError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SafeDBError::Database(format!("failed to create database directory: {e}"))
            })?;
        }
        let db = Database::create(path).map_err(|e| SafeDBError::Database(e.to_string()))?;

        // Ensure the table exists.
        let txn = db.begin_write().map_err(|e| SafeDBError::Database(e.to_string()))?;
        txn.open_table(SAFE_HEADS).map_err(|e| SafeDBError::Database(e.to_string()))?;
        txn.commit().map_err(|e| SafeDBError::Database(e.to_string()))?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Encodes an L1 hash, L2 hash, and L2 number into a 72-byte value.
    ///
    /// Layout: `[L1 hash: 0-31 | L2 hash: 32-63 | L2 number BE: 64-71]`
    ///
    /// Invariant: `l1_hash` must be the hash of the L1 block whose number is
    /// used as the table key. Callers are responsible for maintaining this.
    fn encode_value(l1_hash: B256, l2_hash: B256, l2_number: u64) -> [u8; 72] {
        let mut buf = [0u8; 72];
        buf[..32].copy_from_slice(l1_hash.as_ref());
        buf[32..64].copy_from_slice(l2_hash.as_ref());
        buf[64..72].copy_from_slice(&l2_number.to_be_bytes());
        buf
    }

    /// Decodes a 72-byte value into (L1 hash, L2 hash, L2 number).
    fn decode_value(bytes: &[u8; 72]) -> (B256, B256, u64) {
        let l1_hash = B256::from_slice(&bytes[..32]);
        let l2_hash = B256::from_slice(&bytes[32..64]);
        let l2_number = u64::from_be_bytes(bytes[64..72].try_into().expect("8 bytes"));
        (l1_hash, l2_hash, l2_number)
    }
}

#[async_trait]
impl SafeHeadListener for SafeDB {
    async fn safe_head_updated(
        &self,
        safe_head: L2BlockInfo,
        l1_block: BlockInfo,
    ) -> Result<(), SafeDBError> {
        let db = Arc::clone(&self.db);
        let value = Self::encode_value(
            l1_block.hash,
            safe_head.block_info.hash,
            safe_head.block_info.number,
        );
        let l1_number = l1_block.number;
        let l1_hash = l1_block.hash;
        let l2_number = safe_head.block_info.number;
        let l2_hash = safe_head.block_info.hash;

        tokio::task::spawn_blocking(move || -> Result<(), SafeDBError> {
            let txn = db.begin_write().map_err(|e| SafeDBError::Database(e.to_string()))?;
            {
                let mut table =
                    txn.open_table(SAFE_HEADS).map_err(|e| SafeDBError::Database(e.to_string()))?;
                table
                    .insert(l1_number, &value)
                    .map_err(|e| SafeDBError::Database(e.to_string()))?;
            }
            txn.commit().map_err(|e| SafeDBError::Database(e.to_string()))?;

            tracing::debug!(
                l1_number,
                l1_hash = %l1_hash,
                l2_number,
                l2_hash = %l2_hash,
                "recorded safe head",
            );

            Ok(())
        })
        .await
        .map_err(|e| SafeDBError::Database(format!("blocking task panicked: {e}")))?
    }

    async fn safe_head_reset(&self, reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || -> Result<(), SafeDBError> {
            let txn = db.begin_write().map_err(|e| SafeDBError::Database(e.to_string()))?;
            let (entries_deleted, was_noop) = {
                let mut table =
                    txn.open_table(SAFE_HEADS).map_err(|e| SafeDBError::Database(e.to_string()))?;

                // Forward-scan from reset_origin to find the first entry whose L2 number
                // is >= the reset target. Stops at the boundary without materialising the
                // whole tail — important on long-running nodes where the range can be large.
                //
                // Monotonicity assumption: L2 block numbers increase monotonically with L1
                // block numbers in the DB. The scan stops at the first entry that meets the
                // threshold; everything from that key onward is at-or-above the target.
                let mut first_key: Option<u64> = None;
                for entry in table
                    .range(reset_safe_head.l1_origin.number..)
                    .map_err(|e| SafeDBError::Database(e.to_string()))?
                {
                    let entry = entry.map_err(|e| SafeDBError::Database(e.to_string()))?;
                    let (_, _, l2_number) = Self::decode_value(entry.1.value());
                    if l2_number >= reset_safe_head.block_info.number {
                        first_key = Some(entry.0.value());
                        break;
                    }
                }

                match first_key {
                    Some(fk) => {
                        // Collect only the u64 keys to delete (8 bytes each, not the full
                        // 72-byte values) from first_key onward.
                        let keys_to_delete: Vec<u64> = table
                            .range(fk..)
                            .map_err(|e| SafeDBError::Database(e.to_string()))?
                            .map(|e| {
                                e.map(|e| e.0.value())
                                    .map_err(|e| SafeDBError::Database(e.to_string()))
                            })
                            .collect::<Result<_, _>>()?;
                        let deleted = keys_to_delete.len();
                        for key in keys_to_delete {
                            table.remove(key).map_err(|e| SafeDBError::Database(e.to_string()))?;
                        }

                        // Re-anchor at the reset's stated L1 origin (not fk, which may be
                        // a later L1 block when the reset origin falls in a gap between entries).
                        let value = Self::encode_value(
                            reset_safe_head.l1_origin.hash,
                            reset_safe_head.block_info.hash,
                            reset_safe_head.block_info.number,
                        );
                        table
                            .insert(reset_safe_head.l1_origin.number, &value)
                            .map_err(|e| SafeDBError::Database(e.to_string()))?;
                        (deleted, false)
                    }
                    None => {
                        // Every entry in the range `[reset_origin..]` has an L2 number
                        // *below* the reset target, meaning there is nothing to truncate.
                        // Note: entries before `reset_origin` are unaffected regardless.
                        (0, true)
                    }
                }
            };
            txn.commit().map_err(|e| SafeDBError::Database(e.to_string()))?;

            tracing::debug!(
                l1_origin = reset_safe_head.l1_origin.number,
                l2_number = reset_safe_head.block_info.number,
                l2_hash = %reset_safe_head.block_info.hash,
                entries_deleted,
                was_noop,
                "reset safe head",
            );

            Ok(())
        })
        .await
        .map_err(|e| SafeDBError::Database(format!("blocking task panicked: {e}")))?
    }
}

#[async_trait]
impl SafeDBReader for SafeDB {
    async fn safe_head_at_l1(&self, l1_block_num: u64) -> Result<SafeHeadResponse, SafeDBError> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || -> Result<SafeHeadResponse, SafeDBError> {
            let txn = db.begin_read().map_err(|e| SafeDBError::Database(e.to_string()))?;
            let table =
                txn.open_table(SAFE_HEADS).map_err(|e| SafeDBError::Database(e.to_string()))?;

            // Find the highest entry at or before l1_block_num.
            let entry = table
                .range(..=l1_block_num)
                .map_err(|e| SafeDBError::Database(e.to_string()))?
                .next_back()
                .transpose()
                .map_err(|e| SafeDBError::Database(e.to_string()))?;

            match entry {
                Some(guard) => {
                    let l1_number = guard.0.value();
                    let (l1_hash, l2_hash, l2_number) = Self::decode_value(guard.1.value());
                    Ok(SafeHeadResponse {
                        l1_block: BlockNumHash { number: l1_number, hash: l1_hash },
                        safe_head: BlockNumHash { number: l2_number, hash: l2_hash },
                    })
                }
                None => Err(SafeDBError::NotFound),
            }
        })
        .await
        .map_err(|e| SafeDBError::Database(format!("blocking task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::*;
    use crate::DisabledSafeDB;

    /// Helper to create a [`BlockInfo`] for testing.
    fn block_info(hash: u8, number: u64) -> BlockInfo {
        BlockInfo {
            hash: B256::from([hash; 32]),
            number,
            parent_hash: B256::ZERO,
            timestamp: number * 12,
        }
    }

    /// Helper to create an [`L2BlockInfo`] for testing.
    fn l2_block_info(
        hash: u8,
        number: u64,
        l1_origin_hash: u8,
        l1_origin_number: u64,
    ) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::from([hash; 32]),
                number,
                parent_hash: B256::ZERO,
                timestamp: number * 2,
            },
            l1_origin: BlockNumHash {
                number: l1_origin_number,
                hash: B256::from([l1_origin_hash; 32]),
            },
            seq_num: 0,
        }
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let l1_hash = B256::from([0xAA; 32]);
        let l2_hash = B256::from([0xBB; 32]);
        let l2_number = 42u64;

        let encoded = SafeDB::encode_value(l1_hash, l2_hash, l2_number);
        let (dec_l1, dec_l2, dec_num) = SafeDB::decode_value(&encoded);

        assert_eq!(dec_l1, l1_hash);
        assert_eq!(dec_l2, l2_hash);
        assert_eq!(dec_num, l2_number);
    }

    #[tokio::test]
    async fn test_write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        let l1 = block_info(0x11, 100);
        let l2 = l2_block_info(0x22, 200, 0x11, 100);

        db.safe_head_updated(l2, l1).await.unwrap();

        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.l1_block.number, 100);
        assert_eq!(resp.l1_block.hash, B256::from([0x11; 32]));
        assert_eq!(resp.safe_head.number, 200);
        assert_eq!(resp.safe_head.hash, B256::from([0x22; 32]));
    }

    #[tokio::test]
    async fn test_safe_head_at_l1_returns_highest_at_or_before() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        // Insert entries at L1 blocks 100, 200, 300.
        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 100), block_info(0x01, 100))
            .await
            .unwrap();
        db.safe_head_updated(l2_block_info(0xBB, 2000, 0x02, 200), block_info(0x02, 200))
            .await
            .unwrap();
        db.safe_head_updated(l2_block_info(0xCC, 3000, 0x03, 300), block_info(0x03, 300))
            .await
            .unwrap();

        // Query at 150 — should return the entry at 100.
        let resp = db.safe_head_at_l1(150).await.unwrap();
        assert_eq!(resp.l1_block.number, 100);
        assert_eq!(resp.safe_head.number, 1000);

        // Query at 200 — should return the entry at 200 exactly.
        let resp = db.safe_head_at_l1(200).await.unwrap();
        assert_eq!(resp.l1_block.number, 200);
        assert_eq!(resp.safe_head.number, 2000);

        // Query at 999 — should return the entry at 300.
        let resp = db.safe_head_at_l1(999).await.unwrap();
        assert_eq!(resp.l1_block.number, 300);
        assert_eq!(resp.safe_head.number, 3000);
    }

    #[tokio::test]
    async fn test_safe_head_at_l1_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        // Empty DB should return NotFound.
        let err = db.safe_head_at_l1(100).await.unwrap_err();
        assert!(matches!(err, SafeDBError::NotFound));

        // Insert at L1=200, query at L1=100 should still be NotFound.
        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 200), block_info(0x01, 200))
            .await
            .unwrap();
        let err = db.safe_head_at_l1(100).await.unwrap_err();
        assert!(matches!(err, SafeDBError::NotFound));
    }

    #[tokio::test]
    async fn test_truncate_on_reset() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        // Insert entries at L1=100 (L2=1000), L1=200 (L2=2000), L1=300 (L2=3000).
        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 100), block_info(0x01, 100))
            .await
            .unwrap();
        db.safe_head_updated(l2_block_info(0xBB, 2000, 0x02, 200), block_info(0x02, 200))
            .await
            .unwrap();
        db.safe_head_updated(l2_block_info(0xCC, 3000, 0x03, 300), block_info(0x03, 300))
            .await
            .unwrap();

        // Reset to L2=1500 with L1 origin at 200.
        // This should find the entry at L1=200 (L2=2000 >= 1500), delete entries at 200 and 300,
        // and re-write L1=200 with the reset values.
        let reset = l2_block_info(0xDD, 1500, 0x02, 200);
        db.safe_head_reset(reset).await.unwrap();

        // Entry at L1=100 should still exist.
        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.safe_head.number, 1000);

        // Entry at L1=200 should now point to the reset safe head.
        let resp = db.safe_head_at_l1(200).await.unwrap();
        assert_eq!(resp.safe_head.number, 1500);
        assert_eq!(resp.safe_head.hash, B256::from([0xDD; 32]));

        // Entry at L1=300 should be gone; querying 300 returns the reset entry at 200.
        let resp = db.safe_head_at_l1(300).await.unwrap();
        assert_eq!(resp.l1_block.number, 200);
        assert_eq!(resp.safe_head.number, 1500);
    }

    #[tokio::test]
    async fn test_truncate_on_reset_before_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        // Insert at L1=100 (L2=1000).
        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 100), block_info(0x01, 100))
            .await
            .unwrap();

        // Reset to L2=500 with L1 origin at 50 — before any entry.
        // The entry at L1=100 has L2=1000 >= 500, so it is deleted.
        // The reset anchor is written at the reset's l1_origin (50), not at 100.
        let reset = l2_block_info(0xDD, 500, 0x05, 50);
        db.safe_head_reset(reset).await.unwrap();

        // The reset should be anchored at L1=50.
        let resp = db.safe_head_at_l1(50).await.unwrap();
        assert_eq!(resp.l1_block.number, 50);
        assert_eq!(resp.safe_head.number, 500);
        assert_eq!(resp.safe_head.hash, B256::from([0xDD; 32]));

        // L1=100 was deleted; querying it returns the anchor at L1=50.
        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.l1_block.number, 50);
        assert_eq!(resp.safe_head.number, 500);
    }

    #[tokio::test]
    async fn test_truncate_on_reset_after_last_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        // Insert at L1=100 (L2=1000).
        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 100), block_info(0x01, 100))
            .await
            .unwrap();

        // Reset to L2=2000 with L1 origin at 200 — after all entries.
        // No entries have L2 >= 2000 in the range [200..], so this is a no-op.
        let reset = l2_block_info(0xDD, 2000, 0x02, 200);
        db.safe_head_reset(reset).await.unwrap();

        // Original entry should still exist.
        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.safe_head.number, 1000);
        assert_eq!(resp.safe_head.hash, B256::from([0xAA; 32]));
    }

    /// Verify that a reset whose L1 origin falls in a **gap** between two stored
    /// entries creates a synthetic anchor at the reset origin, not at the first
    /// matching entry.
    ///
    /// Setup: entries at L1=100 (L2=1000) and L1=300 (L2=3000).
    /// Reset:  L1 origin=150, L2 target=1500.
    ///
    /// Expected: entry at L1=300 (L2=3000 >= 1500) is deleted; a new anchor is
    /// written at L1=150 (not at L1=300).  Entry at L1=100 is unchanged.
    #[tokio::test]
    async fn test_reset_creates_synthetic_anchor_in_gap() {
        let dir = tempfile::tempdir().unwrap();
        let db = SafeDB::open(dir.path().join("test.redb")).unwrap();

        db.safe_head_updated(l2_block_info(0xAA, 1000, 0x01, 100), block_info(0x01, 100))
            .await
            .unwrap();
        db.safe_head_updated(l2_block_info(0xCC, 3000, 0x03, 300), block_info(0x03, 300))
            .await
            .unwrap();

        // Reset with L1 origin in the gap (150) and L2 target between entries (1500).
        let reset = l2_block_info(0xDD, 1500, 0x02, 150);
        db.safe_head_reset(reset).await.unwrap();

        // Entry at L1=100 is untouched.
        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.safe_head.number, 1000);

        // Synthetic anchor at L1=150 (the reset's stated origin).
        let resp = db.safe_head_at_l1(150).await.unwrap();
        assert_eq!(resp.l1_block.number, 150);
        assert_eq!(resp.safe_head.number, 1500);
        assert_eq!(resp.safe_head.hash, B256::from([0xDD; 32]));

        // Querying L1=200 (between anchor and deleted entry) resolves to L1=150.
        let resp = db.safe_head_at_l1(200).await.unwrap();
        assert_eq!(resp.l1_block.number, 150);
        assert_eq!(resp.safe_head.number, 1500);

        // L1=300 was deleted; queries resolve to the L1=150 anchor.
        let resp = db.safe_head_at_l1(300).await.unwrap();
        assert_eq!(resp.l1_block.number, 150);
        assert_eq!(resp.safe_head.number, 1500);
    }

    /// Verify that data written to a [`SafeDB`] survives closing and reopening
    /// the underlying redb file.
    #[tokio::test]
    async fn test_persistence_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        {
            let db = SafeDB::open(&path).unwrap();
            db.safe_head_updated(l2_block_info(0xBB, 200, 0x11, 100), block_info(0x11, 100))
                .await
                .unwrap();
        } // db and its redb Database are dropped here

        // Reopen and verify the entry is still present.
        let db = SafeDB::open(&path).unwrap();
        let resp = db.safe_head_at_l1(100).await.unwrap();
        assert_eq!(resp.l1_block.number, 100);
        assert_eq!(resp.l1_block.hash, B256::from([0x11; 32]));
        assert_eq!(resp.safe_head.number, 200);
        assert_eq!(resp.safe_head.hash, B256::from([0xBB; 32]));
    }

    #[tokio::test]
    async fn test_disabled_db_is_noop() {
        let disabled = DisabledSafeDB;

        let l1 = block_info(0x11, 100);
        let l2 = l2_block_info(0x22, 200, 0x11, 100);

        disabled.safe_head_updated(l2, l1).await.unwrap();
        disabled.safe_head_reset(l2).await.unwrap();

        let err = disabled.safe_head_at_l1(100).await.unwrap_err();
        assert!(matches!(err, SafeDBError::Disabled));
    }
}
