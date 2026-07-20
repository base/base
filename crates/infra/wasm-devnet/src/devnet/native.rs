use base_action_harness::{
    ActionTestHarness, BatcherConfig, L1MinerConfig, L2Sequencer, SharedL1Chain,
    TestRollupConfigBuilder, TestRollupNode, VerifierPipeline,
};
use base_common_consensus::BaseBlock;
use base_protocol::{BlockInfo, L2BlockInfo};

/// A fully in-process devnet: L1 miner, batcher, sequencer (CL+EL), and validator (CL+EL).
///
/// All components run in the same process using the production action test harness.
/// The sequencer and validator share a [`SharedL1Chain`] for L1 data delivery. The batcher
/// submits L2 blocks to the shared L1 chain, and the validator picks them up through the
/// real derivation pipeline.
///
/// # Architecture
///
/// ```text
/// ┌──────────────────────────────────────────────────────┐
/// │  L1Miner (in-process, no real chain)                │
/// │    │                                                 │
/// │    ▼                                                 │
/// │  Batcher ──> SharedL1Chain <── L2Sequencer (CL+EL)  │
/// │                    │                                 │
/// │                    └──> Validator / TestRollupNode   │
/// │                              (CL+EL, derivation)    │
/// └──────────────────────────────────────────────────────┘
/// ```
///
/// # WASM Status
///
/// The devnet compiles and runs natively. Full WASM compilation is blocked by the
/// `rocksdb` C++ dependency in `reth-provider`. Once a no-op or in-memory RocksDB
/// implementation is wired (the companion `base-reth-mem-db` crate already provides
/// the MDBX replacement), this crate becomes the browser-side entry point.
///
/// # Example
///
/// ```rust,no_run
/// # async fn run() {
/// use base_wasm_devnet::Devnet;
///
/// let mut devnet = Devnet::new().await;
/// devnet.mine_l1_blocks(2);
/// let derived = devnet.run_epoch(1, 4).await;
/// assert_eq!(derived, 4);
/// # }
/// ```
#[derive(Debug)]
pub struct Devnet {
    /// The test harness that owns the L1 miner and rollup configuration.
    pub harness: ActionTestHarness,
    /// The sequencer: CL (block building, L1 origin selection) + EL (EVM execution).
    pub sequencer: L2Sequencer,
    /// The validator: CL (derivation pipeline) + EL (EVM execution).
    pub validator: TestRollupNode<VerifierPipeline>,
    /// Shared L1 chain view consumed by both the sequencer and the validator.
    pub l1_chain: SharedL1Chain,
    /// Configuration used when creating new batchers.
    pub batcher_config: BatcherConfig,
}

impl Devnet {
    /// Create a new devnet with all hardforks active from genesis.
    ///
    /// Sets up the rollup config so that the batcher's inbox and sender addresses match
    /// the derivation pipeline's expected values. Initializes the validator's pipeline.
    pub async fn new() -> Self {
        let batcher_config = BatcherConfig::default();
        let rollup_config =
            TestRollupConfigBuilder::base_mainnet(&batcher_config).all_forks_active().build();

        let harness = ActionTestHarness::new(L1MinerConfig::default(), rollup_config);
        let l1_chain = SharedL1Chain::from_blocks(harness.l1.chain().to_vec());

        let mut sequencer = harness.create_l2_sequencer(l1_chain.clone());
        let (mut validator, _) =
            harness.create_test_rollup_node_from_sequencer(&mut sequencer, l1_chain.clone());

        validator.initialize().await;

        Self { harness, sequencer, validator, l1_chain, batcher_config }
    }

    /// Mine `n` L1 blocks and push each to the shared chain so the sequencer and
    /// validator see the new epochs.
    ///
    /// Returns the L1 tip block number after mining.
    pub fn mine_l1_blocks(&mut self, n: u64) -> u64 {
        for _ in 0..n {
            self.harness.mine_and_push(&self.l1_chain);
        }
        self.harness.l1.latest_number()
    }

    /// Build the next L2 block via the sequencer (CL+EL), including one user transaction.
    ///
    /// The block is not submitted to L1 until [`submit_l2_blocks`] is called.
    ///
    /// [`submit_l2_blocks`]: Devnet::submit_l2_blocks
    pub async fn produce_l2_block(&mut self) -> BaseBlock {
        self.sequencer.build_next_block_with_single_transaction().await
    }

    /// Build `n` sequential L2 blocks via the sequencer, each with one user transaction.
    ///
    /// Blocks are not submitted to L1 until [`submit_l2_blocks`] is called.
    ///
    /// [`submit_l2_blocks`]: Devnet::submit_l2_blocks
    pub async fn produce_l2_blocks(&mut self, n: u64) -> Vec<BaseBlock> {
        let mut blocks = Vec::with_capacity(n as usize);
        for _ in 0..n {
            blocks.push(self.sequencer.build_next_block_with_single_transaction().await);
        }
        blocks
    }

    /// Submit `blocks` through the batcher, mine one L1 block to carry the submission,
    /// and push that block to the shared chain so the validator can derive from it.
    ///
    /// Returns the [`BlockInfo`] of the newly mined L1 block.
    pub async fn submit_l2_blocks(&mut self, blocks: Vec<BaseBlock>) -> BlockInfo {
        let l1_chain = self.l1_chain.clone();
        let batcher_config = self.batcher_config.clone();
        self.harness.submit_l2_blocks(&l1_chain, batcher_config, blocks).await
    }

    /// Run the validator's derivation pipeline until it has consumed all available L1 data.
    ///
    /// Returns the number of L2 blocks derived and executed by the validator's EL.
    pub async fn derive_until_idle(&mut self) -> usize {
        self.validator.run_until_idle().await
    }

    /// Run one full epoch:
    ///
    /// 1. Mine `l1_blocks` L1 blocks (push each to shared chain).
    /// 2. Produce `l2_blocks` L2 blocks via the sequencer.
    /// 3. Submit those blocks through the batcher (mines one more L1 block).
    /// 4. Run the validator's derivation pipeline until idle.
    ///
    /// Returns the number of L2 blocks derived by the validator.
    pub async fn run_epoch(&mut self, l1_blocks: u64, l2_blocks: u64) -> usize {
        self.mine_l1_blocks(l1_blocks);
        let blocks = self.produce_l2_blocks(l2_blocks).await;
        self.submit_l2_blocks(blocks).await;
        self.derive_until_idle().await
    }

    /// Return the current unsafe L2 head at the sequencer.
    pub fn sequencer_head(&self) -> L2BlockInfo {
        self.sequencer.head()
    }

    /// Return the current L2 safe head at the validator.
    pub fn validator_safe(&self) -> L2BlockInfo {
        self.validator.l2_safe()
    }

    /// Return the current L2 unsafe head at the validator.
    pub fn validator_unsafe(&self) -> L2BlockInfo {
        self.validator.l2_unsafe()
    }

    /// Return the current L1 chain tip block number.
    pub fn l1_tip_number(&self) -> u64 {
        self.harness.l1.latest_number()
    }
}

#[cfg(test)]
mod tests {
    use super::Devnet;

    #[tokio::test]
    async fn devnet_run_epoch() {
        let mut devnet = Devnet::new().await;

        let derived = devnet.run_epoch(1, 4).await;

        assert!(derived > 0, "validator should derive at least one block");
        assert!(devnet.sequencer_head().block_info.number >= 4);
        assert!(devnet.validator_safe().block_info.number > 0);
    }

    #[tokio::test]
    async fn devnet_multiple_epochs() {
        let mut devnet = Devnet::new().await;

        devnet.run_epoch(1, 2).await;
        devnet.run_epoch(1, 2).await;

        assert!(devnet.sequencer_head().block_info.number >= 4);
        assert!(devnet.l1_tip_number() >= 2);
    }
}
