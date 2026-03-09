use std::sync::Arc;

use alloy_eips::BlockNumHash;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{
    ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider, Batcher, BatcherConfig,
    L1Miner, L1MinerConfig, L2Verifier, MockL2Block, MockL2Source, SharedL1Chain, block_info_from,
};

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

    /// Create an [`L2Verifier`] wired to the harness's L1 chain.
    ///
    /// A [`SharedL1Chain`] is initialised from the miner's current chain and
    /// returned alongside the verifier. Mine new blocks with `l1.mine_block()`
    /// then call `chain.push(l1.tip().clone())` and
    /// `verifier.act_l1_head_signal(block_info).await` to feed them into the
    /// pipeline.
    ///
    /// The pipeline is seeded with the L1 genesis block as its origin and with
    /// the L2 genesis state from `rollup_config`.
    pub fn create_verifier(&self) -> (L2Verifier, SharedL1Chain) {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        let rollup_config = Arc::new(self.rollup_config.clone());
        let l1_chain_config = Arc::new(L1ChainConfig::default());

        let l1_provider = ActionL1ChainProvider::new(chain.clone());
        let dap_source =
            ActionDataSource::new(chain.clone(), self.rollup_config.batch_inbox_address);
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);

        // Seed the pipeline origin from the actual genesis block so parent-hash
        // chaining validates correctly when block 1 is provided.
        let genesis_l1_block = self.l1.chain().first().expect("genesis always present");
        let genesis_l1 = block_info_from(genesis_l1_block);

        let safe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: self.rollup_config.genesis.l2.hash,
                number: self.rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: self.rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash {
                number: genesis_l1.number,
                hash: genesis_l1.hash,
            },
            seq_num: 0,
        };

        let verifier = L2Verifier::new(
            rollup_config,
            l1_chain_config,
            l1_provider,
            dap_source,
            l2_provider,
            safe_head,
            genesis_l1,
        );

        (verifier, chain)
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
