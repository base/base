//! Shared fixtures for [`L1WatcherActor`](crate::L1WatcherActor) tests.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_protocol::BlockInfo;
use futures::Stream;

use crate::{DerivationClientResult, L1WatcherDerivationClient};

/// A boxed, pinned stream of L1 blocks, as consumed by the L1 watcher actor.
pub type BoxedBlockStream = Pin<Box<dyn Stream<Item = BlockInfo> + Unpin + Send>>;

/// Records derivation messages sent by the L1 watcher.
#[derive(Debug, Clone, Default)]
pub struct RecordingDerivationClient {
    heads: Arc<Mutex<Vec<BlockInfo>>>,
    finalized: Arc<Mutex<Vec<BlockInfo>>>,
}

#[async_trait]
impl L1WatcherDerivationClient for RecordingDerivationClient {
    async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
        self.finalized.lock().unwrap().push(block);
        Ok(())
    }

    async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
        self.heads.lock().unwrap().push(block);
        Ok(())
    }
}

impl RecordingDerivationClient {
    /// Returns the L1 heads sent to derivation so far.
    pub fn sent_heads(&self) -> Vec<BlockInfo> {
        self.heads.lock().unwrap().clone()
    }
}

/// Builds a deterministic [`BlockInfo`] for the given block number.
pub fn block_at(number: u64) -> BlockInfo {
    BlockInfo {
        hash: B256::from([number as u8; 32]),
        number,
        parent_hash: B256::ZERO,
        timestamp: number * 12,
    }
}
