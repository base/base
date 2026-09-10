use std::{fmt, sync::Arc};

use crate::{BaseProofsStorageError, BaseProofsStorageResult, BaseProofsStore};

/// A read-only view of the committed proofs-history head.
///
/// The callback erases backend cursor types so consumers can read progress without owning a
/// RocksDB or MDBX transaction, or routing through the debug RPC server.
#[derive(Clone)]
pub struct ProofsProgress {
    latest: Arc<dyn Fn() -> BaseProofsStorageResult<Option<u64>> + Send + Sync>,
}

impl fmt::Debug for ProofsProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ProofsProgress").finish_non_exhaustive()
    }
}

/// Failure reading committed proofs progress.
#[derive(Debug, thiserror::Error)]
pub enum ProofsProgressError {
    /// The proofs store failed.
    #[error(transparent)]
    Storage(#[from] BaseProofsStorageError),
    /// The blocking read task failed.
    #[error(transparent)]
    ReadTask(#[from] tokio::task::JoinError),
}

impl ProofsProgress {
    /// Shares progress reads from the store used by the proofs execution extension.
    pub fn new<S: BaseProofsStore + 'static>(store: Arc<S>) -> Self {
        Self {
            latest: Arc::new(move || {
                store.get_latest_block_number().map(|head| head.map(|(number, _)| number))
            }),
        }
    }

    /// Reads the persisted head off the async executor.
    pub async fn latest(&self) -> Result<Option<u64>, ProofsProgressError> {
        let latest = self.latest.clone();
        Ok(tokio::task::spawn_blocking(move || latest()).await??)
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
    use alloy_primitives::B256;

    use super::*;
    use crate::{BlockStateDiff, InMemoryProofsStorage};

    #[tokio::test]
    async fn progress_tracks_commits_and_unwinds() {
        let store = Arc::new(InMemoryProofsStorage::new());
        let progress = ProofsProgress::new(store.clone());
        assert_eq!(progress.latest().await.unwrap(), None);
        store.set_earliest_block_number(0, B256::ZERO).unwrap();
        let genesis = BlockWithParent::new(B256::ZERO, BlockNumHash::new(0, B256::ZERO));
        store.store_trie_updates(genesis, BlockStateDiff::default()).unwrap();
        let block = BlockWithParent::new(B256::ZERO, BlockNumHash::new(1, B256::repeat_byte(1)));
        store.store_trie_updates(block, BlockStateDiff::default()).unwrap();
        assert_eq!(progress.latest().await.unwrap(), Some(1));
        store.unwind_history(block).unwrap();
        assert_eq!(progress.latest().await.unwrap(), Some(0));
    }
}
