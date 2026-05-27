//! Durable checkpoint database.

use std::{path::Path, sync::Arc};

use alloy_primitives::B256;
use base_consensus_engine::ForkchoiceCheckpointLabel;
use base_protocol::{BlockInfo, L2BlockInfo};
use redb::{Database, TableDefinition};
use tokio::task;

use super::CheckpointError;

const CHECKPOINT_VALUE_LEN: usize = 128;
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
        put_b256(&mut bytes, 0, block.block_info.hash);
        put_u64(&mut bytes, 32, block.block_info.number);
        put_b256(&mut bytes, 40, block.block_info.parent_hash);
        put_u64(&mut bytes, 72, block.block_info.timestamp);
        put_u64(&mut bytes, 80, block.l1_origin.number);
        put_b256(&mut bytes, 88, block.l1_origin.hash);
        put_u64(&mut bytes, 120, block.seq_num);
        bytes
    }

    fn decode(bytes: &[u8; Self::VALUE_LEN]) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: get_b256(bytes, 0),
                number: get_u64(bytes, 32),
                parent_hash: get_b256(bytes, 40),
                timestamp: get_u64(bytes, 72),
            },
            l1_origin: alloy_eips::BlockNumHash {
                number: get_u64(bytes, 80),
                hash: get_b256(bytes, 88),
            },
            seq_num: get_u64(bytes, 120),
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
    bytes[offset..offset + 32].copy_from_slice(value.as_slice());
}

fn put_u64(bytes: &mut [u8; CheckpointDB::VALUE_LEN], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn get_b256(bytes: &[u8; CheckpointDB::VALUE_LEN], offset: usize) -> B256 {
    B256::from_slice(&bytes[offset..offset + 32])
}

fn get_u64(bytes: &[u8; CheckpointDB::VALUE_LEN], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("slice length is 8"))
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_consensus_engine::ForkchoiceCheckpointLabel;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::CheckpointDB;

    #[tokio::test]
    async fn checkpoint_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.redb");
        let checkpoint = L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::with_last_byte(1),
                number: 10,
                parent_hash: B256::with_last_byte(2),
                timestamp: 30,
            },
            l1_origin: BlockNumHash { number: 4, hash: B256::with_last_byte(5) },
            seq_num: 6,
        };

        {
            let db = CheckpointDB::open(&path).unwrap();
            db.update(ForkchoiceCheckpointLabel::Safe, checkpoint).await.unwrap();
        }

        let db = CheckpointDB::open(&path).unwrap();
        let stored = db.checkpoint(ForkchoiceCheckpointLabel::Safe).await.unwrap();
        assert_eq!(stored, Some(checkpoint));
    }
}
