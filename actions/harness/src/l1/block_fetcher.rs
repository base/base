//! In-memory [`L1BlockFetcher`] implementation for action tests.

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_rpc_types_eth::{Block, Filter, Header, Log};
use async_trait::async_trait;
use base_consensus_node::L1BlockFetcher;

use crate::{L1Block, SharedL1Chain};

/// Error type for [`ActionL1BlockFetcher`].
#[derive(Debug, thiserror::Error)]
#[error("action L1 block fetcher error: {0}")]
pub struct ActionL1FetcherError(String);

/// In-memory [`L1BlockFetcher`] backed by [`SharedL1Chain`].
///
/// Used to satisfy the [`L1BlockFetcher`] bound on [`L1WatcherActor`] in
/// action tests without requiring a live Ethereum RPC endpoint.
///
/// - `get_logs` returns an empty `Vec` in all cases. Most action tests do not
///   emit signer-rotation logs, so this is the correct behaviour for the
///   current test suite.
/// - `get_block` looks the block up by number or hash in the shared chain and
///   converts it to an [`alloy_rpc_types_eth::Block`].
///
/// [`L1WatcherActor`]: base_consensus_node::L1WatcherActor
#[derive(Debug, Clone)]
pub struct ActionL1BlockFetcher {
    chain: SharedL1Chain,
}

impl ActionL1BlockFetcher {
    /// Create a new fetcher backed by the given shared chain.
    pub const fn new(chain: SharedL1Chain) -> Self {
        Self { chain }
    }
}

/// Convert an [`L1Block`] into an RPC [`Block`] suitable for provider impls.
pub fn l1_block_to_rpc(b: L1Block) -> Block {
    let hash = b.hash();
    Block { header: Header { inner: b.header, hash, ..Default::default() }, ..Default::default() }
}

#[async_trait]
impl L1BlockFetcher for ActionL1BlockFetcher {
    type Error = ActionL1FetcherError;

    async fn get_logs(&self, _filter: Filter) -> Result<Vec<Log>, Self::Error> {
        // Most action tests do not emit signer-rotation logs. Return empty.
        Ok(vec![])
    }

    async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error> {
        let l1_block = match id {
            BlockId::Hash(hash_id) => self.chain.block_by_hash(hash_id.block_hash),
            BlockId::Number(tag) => match tag {
                BlockNumberOrTag::Number(n) => self.chain.get_block(n),
                BlockNumberOrTag::Latest => self.chain.tip(),
                _ => None,
            },
        };
        Ok(l1_block.map(l1_block_to_rpc))
    }
}
