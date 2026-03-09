use alloy_consensus::{Header, Receipt};
use alloy_primitives::{Address, Bytes, Log, B256};
use base_protocol::BlockInfo;
use tracing::info;

use crate::Action;

/// Convert an [`L1Block`] reference to a [`BlockInfo`].
pub fn block_info_from(block: &L1Block) -> BlockInfo {
    BlockInfo {
        hash: block.hash(),
        number: block.number(),
        parent_hash: block.header.parent_hash,
        timestamp: block.timestamp(),
    }
}

/// Error returned by [`L1Miner::reorg_to`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReorgError {
    /// The requested block number is beyond the current chain tip.
    #[error("cannot reorg to block {requested}: chain tip is at block {tip}")]
    BeyondTip {
        /// The requested reorg target.
        requested: u64,
        /// The current chain tip.
        tip: u64,
    },
}

/// Configuration for the [`L1Miner`].
#[derive(Debug, Clone)]
pub struct L1MinerConfig {
    /// Simulated L1 block time in seconds. Post-merge Ethereum uses 12 s.
    pub block_time: u64,
}

impl Default for L1MinerConfig {
    fn default() -> Self {
        Self { block_time: 12 }
    }
}

/// A batcher transaction to be included in an L1 block.
///
/// Op-stack batchers submit calldata transactions where:
/// - `to` is the batch inbox address from the rollup config
/// - `from` is the batcher's address
/// - `input` is the frame-encoded batch data (version byte + encoded frames)
///
/// In real networks these are signed EIP-1559 transactions. The action
/// harness uses unsigned representations since the derivation pipeline
/// only needs the sender, recipient, and calldata — not a valid signature.
#[derive(Debug, Clone)]
pub struct PendingTx {
    /// Address of the batcher account that submitted this transaction.
    pub from: Address,
    /// The batch inbox address this transaction was sent to.
    pub to: Address,
    /// Raw frame data: `[DERIVATION_VERSION_0] ++ encoded_frames`.
    pub input: Bytes,
}

/// An L1 block produced by the [`L1Miner`].
///
/// Mirrors the structure the derivation pipeline reads from L1: a block
/// header (for timestamp, number, and hash chaining) together with the
/// ordered list of batcher transactions that were submitted before the
/// block was mined, and any receipts emitted during block execution.
#[derive(Debug, Clone)]
pub struct L1Block {
    /// Consensus header.
    pub header: Header,
    /// Batcher transactions included in this block, in submission order.
    pub batcher_txs: Vec<PendingTx>,
    /// Synthetic receipts carrying [`Log`]s that the derivation pipeline
    /// should process (e.g. `ConfigUpdate` logs for [`SystemConfig`] changes).
    ///
    /// In the real chain every transaction produces a receipt; here we bundle
    /// all test-emitted logs into a single receipt for simplicity.
    ///
    /// [`SystemConfig`]: base_consensus_genesis::SystemConfig
    pub receipts: Vec<Receipt>,
}

impl L1Block {
    /// Return the block number.
    pub fn number(&self) -> u64 {
        self.header.number
    }

    /// Return the block timestamp.
    pub fn timestamp(&self) -> u64 {
        self.header.timestamp
    }

    /// Compute the block hash by hashing the header fields.
    ///
    /// Uses alloy's `Header::hash_slow` which hashes the RLP-encoded header.
    pub fn hash(&self) -> B256 {
        self.header.hash_slow()
    }
}

/// Simulated L1 block producer for action tests.
///
/// `L1Miner` maintains an in-memory chain of [`L1Block`]s, starting from a
/// genesis block at number 0, timestamp 0. Each call to [`mine_block`]
/// (or [`Action::act`]) advances the chain by one block, draining any
/// pending batcher transactions into the new block's body.
///
/// The miner also tracks safe and finalized head pointers using fixed
/// offsets that approximate Ethereum's post-merge consensus behaviour:
/// safe lags 32 blocks behind the latest head, and finalized lags 64.
///
/// # Reorgs
///
/// [`reorg_to`] truncates the canonical chain back to a given block number,
/// discarding all later blocks and returning them so tests can inspect which
/// batcher transactions were reorged out. After a reorg the caller can submit
/// new transactions and mine new blocks as normal — the new fork diverges
/// from the reorg point.
///
/// To guarantee that post-reorg blocks have distinct hashes from the original
/// blocks at the same heights (even when their contents happen to be
/// identical), the miner stamps a monotonically increasing `fork_id` into
/// every block's `extra_data`. The `fork_id` increments on each call to
/// `reorg_to`, so blocks mined on the new fork always differ from those on
/// the old one.
///
/// [`mine_block`]: L1Miner::mine_block
/// [`reorg_to`]: L1Miner::reorg_to
#[derive(Debug)]
pub struct L1Miner {
    /// All blocks produced so far, indexed by block number.
    blocks: Vec<L1Block>,
    /// Batcher transactions waiting to be included in the next block.
    pending: Vec<PendingTx>,
    /// Logs to be wrapped into a single receipt in the next mined block.
    pending_logs: Vec<Log>,
    /// Configuration.
    config: L1MinerConfig,
    /// Monotonically increasing fork counter, stamped into every block's
    /// `extra_data` to ensure distinct hashes across forks.
    fork_id: u64,
}

impl L1Miner {
    /// Create a new [`L1Miner`] initialised with a genesis block.
    pub fn new(config: L1MinerConfig) -> Self {
        let genesis = L1Block {
            header: Header { number: 0, timestamp: 0, ..Default::default() },
            batcher_txs: vec![],
            receipts: vec![],
        };
        Self { blocks: vec![genesis], pending: vec![], pending_logs: vec![], config, fork_id: 0 }
    }

    /// Queue a [`Log`] to be included in a synthetic receipt in the next
    /// mined block.
    ///
    /// Logs are wrapped into a single [`Receipt`] during [`mine_block`], which
    /// is how the derivation pipeline receives `ConfigUpdate` events and other
    /// L1-emitted signals.
    ///
    /// [`mine_block`]: L1Miner::mine_block
    pub fn enqueue_log(&mut self, log: Log) {
        self.pending_logs.push(log);
    }

    /// Return the most recently mined block.
    pub fn latest(&self) -> &L1Block {
        // Safety: `blocks` always contains at least the genesis block.
        self.blocks.last().expect("chain is never empty")
    }

    /// Return the block number of the latest head.
    pub fn latest_number(&self) -> u64 {
        self.latest().number()
    }

    /// Return the safe head, lagging 32 blocks behind the latest head.
    ///
    /// If the chain is shorter than 32 blocks, returns the genesis block.
    pub fn safe_head(&self) -> &L1Block {
        let idx = self.blocks.len().saturating_sub(33);
        &self.blocks[idx]
    }

    /// Return the finalized head, lagging 64 blocks behind the latest head.
    ///
    /// If the chain is shorter than 64 blocks, returns the genesis block.
    pub fn finalized_head(&self) -> &L1Block {
        let idx = self.blocks.len().saturating_sub(65);
        &self.blocks[idx]
    }

    /// Return the block at `number`, or `None` if it has not been mined yet.
    pub fn block_by_number(&self, number: u64) -> Option<&L1Block> {
        self.blocks.get(number as usize)
    }

    /// Return a slice over the entire chain.
    pub fn chain(&self) -> &[L1Block] {
        &self.blocks
    }

    /// Reorg the canonical chain back to `number`, discarding all later blocks.
    ///
    /// Returns the discarded blocks in order (lowest number first) so tests
    /// can inspect which batcher transactions were reorged out. The pending
    /// transaction queue is left untouched — the batcher can choose to
    /// resubmit or discard pending frames as appropriate for the scenario.
    ///
    /// After a reorg, `mine_block` builds on top of block `number`. Because
    /// the `fork_id` is incremented, new blocks will have different hashes
    /// than the original blocks at the same heights.
    ///
    /// # Errors
    ///
    /// Returns [`ReorgError::BeyondTip`] if `number` is greater than the
    /// current chain tip. Reorging to block 0 is valid — it keeps only the
    /// immutable genesis block and discards all subsequent blocks.
    pub fn reorg_to(&mut self, number: u64) -> Result<Vec<L1Block>, ReorgError> {
        let tip = self.latest_number();
        if number > tip {
            return Err(ReorgError::BeyondTip { requested: number, tip });
        }

        self.fork_id += 1;
        let discarded: Vec<L1Block> = self.blocks.drain((number as usize + 1)..).collect();

        info!(
            reorg_to = number,
            fork_id = self.fork_id,
            discarded = discarded.len(),
            "L1 reorg"
        );

        Ok(discarded)
    }

    /// Return a slice of all pending transactions waiting to be mined.
    pub fn pending_txs(&self) -> &[PendingTx] {
        &self.pending
    }

    /// Alias for [`latest`] — returns the current chain tip.
    ///
    /// [`latest`]: L1Miner::latest
    pub fn tip(&self) -> &L1Block {
        self.latest()
    }

    /// Enqueue a batcher transaction for inclusion in the next mined block.
    ///
    /// The derivation pipeline filters L1 transactions by comparing the
    /// `to` field against the batch inbox address in the rollup config, and
    /// `from` against the expected batcher address. Both must be set
    /// correctly for the pipeline to pick up the frames.
    pub fn submit_tx(&mut self, tx: PendingTx) {
        self.pending.push(tx);
    }

    /// Mine the next L1 block, consuming all pending batcher transactions.
    ///
    /// The new block's `parent_hash` is set to the previous block's hash,
    /// and its timestamp advances by `block_time` seconds. All currently
    /// pending batcher transactions are drained into the block body.
    pub fn mine_block(&mut self) -> &L1Block {
        let parent = self.latest();
        let parent_hash = parent.hash();
        let number = parent.header.number + 1;
        let timestamp = parent.header.timestamp + self.config.block_time;

        let batcher_txs = core::mem::take(&mut self.pending);
        let logs = core::mem::take(&mut self.pending_logs);
        let receipts = if logs.is_empty() {
            vec![]
        } else {
            vec![Receipt { status: true.into(), cumulative_gas_used: 0, logs }]
        };

        let block = L1Block {
            header: Header {
                number,
                timestamp,
                parent_hash,
                // Approximate a realistic base fee so tests that inspect fee
                // fields don't see a zero value.
                base_fee_per_gas: Some(1_000_000_000),
                // Stamp the current fork_id so that blocks produced on
                // different forks always have distinct hashes, even when
                // their number, timestamp, and parent hash are identical.
                extra_data: Bytes::copy_from_slice(&self.fork_id.to_be_bytes()),
                ..Default::default()
            },
            batcher_txs,
            receipts,
        };

        info!(
            block_number = number,
            timestamp = timestamp,
            pending_txs = block.batcher_txs.len(),
            "mined L1 block"
        );

        self.blocks.push(block);
        self.blocks.last().expect("just pushed")
    }
}

impl Default for L1Miner {
    fn default() -> Self {
        Self::new(L1MinerConfig::default())
    }
}

impl Action for L1Miner {
    /// The block number of the newly mined block.
    type Output = u64;
    type Error = core::convert::Infallible;

    fn act(&mut self) -> Result<u64, core::convert::Infallible> {
        Ok(self.mine_block().number())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn miner() -> L1Miner {
        L1Miner::default()
    }

    #[test]
    fn genesis_block_at_number_zero() {
        let m = miner();
        assert_eq!(m.latest_number(), 0);
        assert_eq!(m.latest().timestamp(), 0);
    }

    #[test]
    fn mine_increments_number_and_timestamp() {
        let mut m = miner();
        m.mine_block();
        assert_eq!(m.latest_number(), 1);
        assert_eq!(m.latest().timestamp(), 12);
        m.mine_block();
        assert_eq!(m.latest_number(), 2);
        assert_eq!(m.latest().timestamp(), 24);
    }

    #[test]
    fn blocks_form_valid_parent_hash_chain() {
        let mut m = miner();
        m.mine_block();
        m.mine_block();
        m.mine_block();
        for i in 1..=3u64 {
            let block = m.block_by_number(i).unwrap();
            let parent = m.block_by_number(i - 1).unwrap();
            assert_eq!(block.header.parent_hash, parent.hash());
        }
    }

    #[test]
    fn safe_and_finalized_head_clamp_at_genesis() {
        let m = miner();
        assert_eq!(m.safe_head().number(), 0);
        assert_eq!(m.finalized_head().number(), 0);
    }

    #[test]
    fn pending_txs_included_in_next_block() {
        let mut m = miner();
        m.submit_tx(PendingTx {
            from: Address::ZERO,
            to: Address::ZERO,
            input: Bytes::from_static(b"\x00hello"),
        });
        m.mine_block();
        assert_eq!(m.latest().batcher_txs.len(), 1);
        assert_eq!(m.latest().batcher_txs[0].input, Bytes::from_static(b"\x00hello"));
    }

    #[test]
    fn pending_txs_cleared_after_mining() {
        let mut m = miner();
        m.submit_tx(PendingTx {
            from: Address::ZERO,
            to: Address::ZERO,
            input: Bytes::new(),
        });
        m.mine_block();
        m.mine_block();
        // Second block should have no batcher txs.
        assert!(m.latest().batcher_txs.is_empty());
    }

    #[test]
    fn act_returns_block_number() {
        let mut m = miner();
        assert_eq!(m.act().unwrap(), 1);
        assert_eq!(m.act().unwrap(), 2);
    }

    // ── reorg tests ──────────────────────────────────────────────────────────

    #[test]
    fn reorg_truncates_chain() {
        let mut m = miner();
        m.mine_block(); // 1
        m.mine_block(); // 2
        m.mine_block(); // 3

        let discarded = m.reorg_to(1).unwrap();
        assert_eq!(discarded.len(), 2);
        assert_eq!(discarded[0].number(), 2);
        assert_eq!(discarded[1].number(), 3);
        assert_eq!(m.latest_number(), 1);
    }

    #[test]
    fn reorg_returns_batcher_txs_from_discarded_blocks() {
        let mut m = miner();
        m.mine_block(); // 1 — empty

        m.submit_tx(PendingTx {
            from: Address::ZERO,
            to: Address::ZERO,
            input: Bytes::from_static(b"\x00batch"),
        });
        m.mine_block(); // 2 — contains the batch tx

        m.mine_block(); // 3 — empty

        let discarded = m.reorg_to(1).unwrap();
        // Block 2 contained the batch, block 3 was empty.
        assert_eq!(discarded[0].number(), 2);
        assert_eq!(discarded[0].batcher_txs.len(), 1);
        assert_eq!(discarded[0].batcher_txs[0].input, Bytes::from_static(b"\x00batch"));
        assert_eq!(discarded[1].number(), 3);
        assert!(discarded[1].batcher_txs.is_empty());
    }

    #[test]
    fn reorg_to_tip_is_no_op() {
        let mut m = miner();
        m.mine_block();
        m.mine_block();

        let discarded = m.reorg_to(2).unwrap();
        assert!(discarded.is_empty());
        assert_eq!(m.latest_number(), 2);
    }

    #[test]
    fn post_reorg_blocks_have_distinct_hashes() {
        let mut m = miner();
        m.mine_block(); // block 1 on fork 0
        let original_hash_1 = m.block_by_number(1).unwrap().hash();

        // Reorg all the way back to genesis (now allowed); mine a new block 1 on fork 1.
        m.reorg_to(0).unwrap(); // fork_id → 1; chain back to [genesis]
        m.mine_block();         // new block 1 on fork 1

        let new_hash_1 = m.block_by_number(1).unwrap().hash();
        assert_ne!(
            new_hash_1, original_hash_1,
            "block 1 on fork 1 must have a different hash than block 1 on fork 0",
        );
    }

    #[test]
    fn mine_after_reorg_builds_valid_parent_chain() {
        let mut m = miner();
        m.mine_block();
        m.mine_block();
        m.mine_block();

        m.reorg_to(1).unwrap();
        m.mine_block(); // new 2
        m.mine_block(); // new 3

        for i in 1..=3u64 {
            let block = m.block_by_number(i).unwrap();
            let parent = m.block_by_number(i - 1).unwrap();
            assert_eq!(block.header.parent_hash, parent.hash(), "parent chain broken at {i}");
        }
    }

    #[test]
    fn reorg_beyond_tip_returns_error() {
        let mut m = miner();
        m.mine_block();
        assert!(matches!(m.reorg_to(5), Err(ReorgError::BeyondTip { requested: 5, tip: 1 })));
    }

    #[test]
    fn reorg_to_genesis_discards_all_non_genesis_blocks() {
        let mut m = miner();
        m.mine_block(); // block 1
        m.mine_block(); // block 2
        let discarded = m.reorg_to(0).expect("reorg to genesis should succeed");
        assert_eq!(discarded.len(), 2, "blocks 1 and 2 should be discarded");
        assert_eq!(m.latest_number(), 0, "only genesis remains");
    }

    #[test]
    fn safe_head_adjusts_after_reorg() {
        let mut m = miner();
        // Mine enough blocks for safe head to advance.
        for _ in 0..40 {
            m.mine_block();
        }
        assert!(m.safe_head().number() > 0);

        // Reorg back to block 5 — safe head must recalculate.
        m.reorg_to(5).unwrap();
        assert_eq!(m.safe_head().number(), 0, "safe head should clamp to genesis after short reorg");
        assert_eq!(m.latest_number(), 5);
    }
}
