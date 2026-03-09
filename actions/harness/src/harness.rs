use base_consensus_genesis::RollupConfig;

use crate::{Batcher, BatcherConfig, L1Miner, L1MinerConfig, MockL2Block, MockL2Source};

/// Top-level test harness that owns all actors for a single action test.
///
/// `ActionTestHarness` is the entry point for writing action tests. It holds
/// the [`L1Miner`], the [`MockL2Source`], and the [`RollupConfig`] that
/// actors share. Convenience methods let tests drive the harness at a high
/// level; the underlying actors are also exposed as public fields so tests
/// can call actor methods directly when they need finer control.
///
/// # Example
///
/// ```rust
/// use base_action_harness::ActionTestHarness;
///
/// let mut h = ActionTestHarness::default();
/// h.mine_l1_blocks(3);
/// assert_eq!(h.l1.latest_number(), 3);
/// ```
#[derive(Debug)]
pub struct ActionTestHarness {
    /// The simulated L1 chain.
    pub l1: L1Miner,
    /// Pre-loaded L2 blocks available for a batcher to consume.
    pub l2: MockL2Source,
    /// The rollup configuration shared by all actors.
    pub rollup_config: RollupConfig,
}

impl ActionTestHarness {
    /// Create a harness with the given configurations.
    pub fn new(l1_config: L1MinerConfig, rollup_config: RollupConfig) -> Self {
        Self { l1: L1Miner::new(l1_config), l2: MockL2Source::new(), rollup_config }
    }

    /// Mine `n` L1 blocks and return the latest block number after mining.
    pub fn mine_l1_blocks(&mut self, n: u64) -> u64 {
        for _ in 0..n {
            self.l1.mine_block();
        }
        self.l1.latest_number()
    }

    /// Generate `count` sequential mock L2 blocks and enqueue them in the
    /// L2 source.
    ///
    /// Blocks are numbered starting from the current queue length, and
    /// timestamps use the rollup config's block time.
    pub fn generate_l2_blocks(&mut self, count: u64) {
        let start = self.l2.remaining() as u64;
        let block_time = self.rollup_config.block_time;
        self.l2.generate(start, start * block_time, block_time, count);
    }

    /// Push a single hand-crafted L2 block into the source.
    pub fn push_l2_block(&mut self, block: MockL2Block) {
        self.l2.push(block);
    }

    /// Create a [`Batcher`] that drains the harness's L2 source.
    ///
    /// The L2 source is moved out of the harness into the batcher so the
    /// batcher owns the block queue. After the batcher is dropped, the harness
    /// L2 source will be empty; push new blocks before creating another
    /// batcher if needed.
    pub fn create_batcher(&mut self, config: BatcherConfig) -> Batcher<'_, MockL2Source> {
        let l2_source = core::mem::take(&mut self.l2);
        Batcher::new(&mut self.l1, l2_source, &self.rollup_config, config)
    }
}

impl Default for ActionTestHarness {
    fn default() -> Self {
        Self::new(L1MinerConfig::default(), RollupConfig::default())
    }
}
