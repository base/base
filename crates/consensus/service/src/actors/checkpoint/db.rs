//! Durable checkpoint database.

use std::{path::Path, sync::Arc};

use alloy_primitives::B256;
use base_consensus_engine::ForkchoiceCheckpointLabel;
use base_protocol::{BlockInfo, L2BlockInfo};
use redb::{Database, TableDefinition};
use tokio::task;

use super::CheckpointError;

/// On-disk size of a [`B256`] hash.
const B256_LEN: usize = 32;
/// On-disk size of a `u64`.
const U64_LEN: usize = 8;

/// Byte offsets of each [`L2BlockInfo`] field within the encoded payload, derived from the
/// field order in [`CheckpointDB::encode`].
///
/// Centralising the offsets here means a field change is a single-point edit: extend or
/// reorder the chain, and every reader and writer (and the layout-pinning test) is updated
/// in lockstep instead of through a fan-out of hand-counted magic numbers.
const HASH_OFFSET: usize = 0;
const NUMBER_OFFSET: usize = HASH_OFFSET + B256_LEN;
const PARENT_HASH_OFFSET: usize = NUMBER_OFFSET + U64_LEN;
const TIMESTAMP_OFFSET: usize = PARENT_HASH_OFFSET + B256_LEN;
const L1_ORIGIN_NUMBER_OFFSET: usize = TIMESTAMP_OFFSET + U64_LEN;
const L1_ORIGIN_HASH_OFFSET: usize = L1_ORIGIN_NUMBER_OFFSET + U64_LEN;
const SEQ_NUM_OFFSET: usize = L1_ORIGIN_HASH_OFFSET + B256_LEN;
const PAYLOAD_END: usize = SEQ_NUM_OFFSET + U64_LEN;

/// Encoded checkpoint value length. Fixed at 128 bytes so the on-disk schema is identical
/// to the original layout shipped in #2698; any existing databases continue to round-trip
/// without migration.
///
/// The current payload (`PAYLOAD_END`) fills this slot exactly — there is no spare capacity.
/// Any new field appended to [`L2BlockInfo`] will require expanding this constant, which
/// changes the redb table type (`&[u8; 128]`) and is an on-disk-breaking migration that
/// must be handled deliberately.
const CHECKPOINT_VALUE_LEN: usize = 128;

const _: () = assert!(
    PAYLOAD_END <= CHECKPOINT_VALUE_LEN,
    "L2BlockInfo encoding overflows the redb value slot; expanding CHECKPOINT_VALUE_LEN \
     is a breaking on-disk change"
);

const CHECKPOINTS: TableDefinition<'_, u8, &[u8; CHECKPOINT_VALUE_LEN]> =
    TableDefinition::new("checkpoints");

/// Redb-backed checkpoint database.
#[derive(Debug, Clone)]
pub struct CheckpointDB {
    db: Arc<Database>,
}

impl CheckpointDB {
    /// Encoded [`L2BlockInfo`] length.
    pub const VALUE_LEN: usize = CHECKPOINT_VALUE_LEN;

    /// Opens a checkpoint database at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CheckpointError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CheckpointError::Database(format!("failed to create directory: {e}"))
            })?;
        }

        let db = Database::create(path).map_err(|e| CheckpointError::Database(e.to_string()))?;
        let txn = db.begin_write().map_err(|e| CheckpointError::Database(e.to_string()))?;
        txn.open_table(CHECKPOINTS).map_err(|e| CheckpointError::Database(e.to_string()))?;
        txn.commit().map_err(|e| CheckpointError::Database(e.to_string()))?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Stores a checkpoint for the given label.
    pub async fn update(
        &self,
        label: ForkchoiceCheckpointLabel,
        block: L2BlockInfo,
    ) -> Result<(), CheckpointError> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let txn = db.begin_write().map_err(|e| CheckpointError::Database(e.to_string()))?;
            {
                let mut table = txn
                    .open_table(CHECKPOINTS)
                    .map_err(|e| CheckpointError::Database(e.to_string()))?;
                table
                    .insert(label_key(label), &Self::encode(block))
                    .map_err(|e| CheckpointError::Database(e.to_string()))?;
            }
            txn.commit().map_err(|e| CheckpointError::Database(e.to_string()))
        })
        .await
        .map_err(|e| CheckpointError::Database(format!("blocking task panicked: {e}")))?
    }

    /// Returns a checkpoint for the given label.
    pub async fn checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
    ) -> Result<Option<L2BlockInfo>, CheckpointError> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let txn = db.begin_read().map_err(|e| CheckpointError::Database(e.to_string()))?;
            let table = txn
                .open_table(CHECKPOINTS)
                .map_err(|e| CheckpointError::Database(e.to_string()))?;
            Ok(table
                .get(label_key(label))
                .map_err(|e| CheckpointError::Database(e.to_string()))?
                .map(|value| Self::decode(value.value())))
        })
        .await
        .map_err(|e| CheckpointError::Database(format!("blocking task panicked: {e}")))?
    }

    fn encode(block: L2BlockInfo) -> [u8; Self::VALUE_LEN] {
        let mut bytes = [0; Self::VALUE_LEN];
        put_b256(&mut bytes, HASH_OFFSET, block.block_info.hash);
        put_u64(&mut bytes, NUMBER_OFFSET, block.block_info.number);
        put_b256(&mut bytes, PARENT_HASH_OFFSET, block.block_info.parent_hash);
        put_u64(&mut bytes, TIMESTAMP_OFFSET, block.block_info.timestamp);
        put_u64(&mut bytes, L1_ORIGIN_NUMBER_OFFSET, block.l1_origin.number);
        put_b256(&mut bytes, L1_ORIGIN_HASH_OFFSET, block.l1_origin.hash);
        put_u64(&mut bytes, SEQ_NUM_OFFSET, block.seq_num);
        bytes
    }

    fn decode(bytes: &[u8; Self::VALUE_LEN]) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: get_b256(bytes, HASH_OFFSET),
                number: get_u64(bytes, NUMBER_OFFSET),
                parent_hash: get_b256(bytes, PARENT_HASH_OFFSET),
                timestamp: get_u64(bytes, TIMESTAMP_OFFSET),
            },
            l1_origin: alloy_eips::BlockNumHash {
                number: get_u64(bytes, L1_ORIGIN_NUMBER_OFFSET),
                hash: get_b256(bytes, L1_ORIGIN_HASH_OFFSET),
            },
            seq_num: get_u64(bytes, SEQ_NUM_OFFSET),
        }
    }
}

const fn label_key(label: ForkchoiceCheckpointLabel) -> u8 {
    match label {
        ForkchoiceCheckpointLabel::Safe => 0,
        ForkchoiceCheckpointLabel::Finalized => 1,
    }
}

fn put_b256(bytes: &mut [u8; CheckpointDB::VALUE_LEN], offset: usize, value: B256) {
    bytes[offset..offset + B256_LEN].copy_from_slice(value.as_slice());
}

fn put_u64(bytes: &mut [u8; CheckpointDB::VALUE_LEN], offset: usize, value: u64) {
    bytes[offset..offset + U64_LEN].copy_from_slice(&value.to_be_bytes());
}

fn get_b256(bytes: &[u8; CheckpointDB::VALUE_LEN], offset: usize) -> B256 {
    B256::from_slice(&bytes[offset..offset + B256_LEN])
}

fn get_u64(bytes: &[u8; CheckpointDB::VALUE_LEN], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + U64_LEN].try_into().expect("slice length is 8"))
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_consensus_engine::ForkchoiceCheckpointLabel;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::CheckpointDB;

    fn sample_checkpoint() -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(1),
                number: 10,
                parent_hash: B256::with_last_byte(2),
                timestamp: 30,
            },
            l1_origin: BlockNumHash { number: 4, hash: B256::with_last_byte(5) },
            seq_num: 6,
        }
    }

    /// Hand-constructed bytes of the v0.13 / #2698 layout. Any encoder change that perturbs
    /// these bytes \u2014 or any decoder change that fails to reconstruct
    /// [`sample_checkpoint`] from them \u2014 will fail the round-trip tests below and surface
    /// loudly instead of silently producing wrong records on already-deployed databases.
    fn legacy_encoded_bytes() -> [u8; CheckpointDB::VALUE_LEN] {
        let mut bytes = [0u8; CheckpointDB::VALUE_LEN];
        bytes[31] = 1;
        bytes[32..40].copy_from_slice(&10u64.to_be_bytes());
        bytes[71] = 2;
        bytes[72..80].copy_from_slice(&30u64.to_be_bytes());
        bytes[80..88].copy_from_slice(&4u64.to_be_bytes());
        bytes[119] = 5;
        bytes[120..128].copy_from_slice(&6u64.to_be_bytes());
        bytes
    }

    #[tokio::test]
    async fn checkpoint_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.redb");
        let checkpoint = sample_checkpoint();

        {
            let db = CheckpointDB::open(&path).unwrap();
            db.update(ForkchoiceCheckpointLabel::Safe, checkpoint).await.unwrap();
        }

        let db = CheckpointDB::open(&path).unwrap();
        let stored = db.checkpoint(ForkchoiceCheckpointLabel::Safe).await.unwrap();
        assert_eq!(stored, Some(checkpoint));
    }

    #[test]
    fn encode_matches_pinned_layout() {
        assert_eq!(
            CheckpointDB::encode(sample_checkpoint()),
            legacy_encoded_bytes(),
            "L2BlockInfo on-disk encoding has changed; databases written by earlier builds \
             will be mis-decoded"
        );
    }

    #[test]
    fn decode_round_trips_pinned_layout() {
        assert_eq!(CheckpointDB::decode(&legacy_encoded_bytes()), sample_checkpoint());
    }
}
