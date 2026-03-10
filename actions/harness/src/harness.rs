use std::sync::Arc;

use alloy_eips::BlockNumHash;
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{
    ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider, Batcher, BatcherConfig,
    L1Miner, L1MinerConfig, L2BlockBuilder, L2BlockProvider, L2Verifier, SharedL1Chain,
    block_info_from,
};

/// Top-level test harness that owns all actors for a single action test.
///
/// `ActionTestHarness` is the entry point for writing action tests. It holds
/// the [`L1Miner`] and the [`RollupConfig`] shared by all actors. Tests drive
/// the harness step-by-step using the public actor APIs.
///
/// L2 blocks are produced by an [`L2BlockBuilder`] obtained via
/// [`create_l2_builder`]. Blocks contain real L1-info deposit transactions and
/// real signed EIP-1559 user transactions — no simplified mock types.
///
/// [`create_l2_builder`]: ActionTestHarness::create_l2_builder
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
    /// The rollup configuration shared by all actors.
    pub rollup_config: RollupConfig,
}

impl ActionTestHarness {
    /// Create a harness with the given configurations.
    pub fn new(l1_config: L1MinerConfig, rollup_config: RollupConfig) -> Self {
        Self { l1: L1Miner::new(l1_config), rollup_config }
    }

    /// Mine `n` L1 blocks and return the latest block number after mining.
    pub fn mine_l1_blocks(&mut self, n: u64) -> u64 {
        for _ in 0..n {
            self.l1.mine_block();
        }
        self.l1.latest_number()
    }

    /// Create a [`Batcher`] backed by the supplied L2 block source.
    ///
    /// Unlike the previous `create_batcher` that consumed an internal
    /// `MockL2Source`, this accepts any [`L2BlockProvider`] so tests can wire
    /// an [`L2BlockBuilder`] or a hand-rolled source directly.
    pub fn create_batcher<S: L2BlockProvider>(
        &mut self,
        source: S,
        config: BatcherConfig,
    ) -> Batcher<'_, S> {
        Batcher::new(&mut self.l1, source, &self.rollup_config, config)
    }

    /// Create an [`L2BlockBuilder`] starting from L2 genesis, wired to a
    /// snapshot of the current L1 chain.
    ///
    /// The returned builder generates real [`OpBlock`]s with a proper L1-info
    /// deposit transaction (first tx) and signed EIP-1559 user transactions.
    /// Call `build_next_block()` once per L2 block to advance the builder.
    ///
    /// After mining new L1 blocks, push them to the [`SharedL1Chain`] returned
    /// alongside the verifier so the builder sees the updated epochs.
    pub fn create_l2_builder(&self, l1_chain: SharedL1Chain) -> L2BlockBuilder {
        let l1_genesis_hash = l1_chain.get_block(0).map(|b| b.hash()).unwrap_or_default();

        let genesis_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: self.rollup_config.genesis.l2.hash,
                number: self.rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: self.rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash { number: 0, hash: l1_genesis_hash },
            seq_num: 0,
        };

        let system_config = self.rollup_config.genesis.system_config.unwrap_or_default();

        L2BlockBuilder::new(genesis_head, l1_chain, self.rollup_config.clone(), system_config)
    }

    /// Create an [`L2Verifier`] wired to the harness's L1 chain.
    ///
    /// A [`SharedL1Chain`] is initialised from the miner's current chain and
    /// returned alongside the verifier. Mine new blocks with `l1.mine_block()`
    /// then call `chain.push(l1.tip().clone())` and
    /// `verifier.act_l1_head_signal(block_info).await` to feed them into the
    /// pipeline.
    pub fn create_verifier(&self) -> (L2Verifier, SharedL1Chain) {
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);
        self.create_verifier_with_l2_provider(l2_provider)
    }

    /// Create an [`L2Verifier`] using a caller-supplied [`ActionL2ChainProvider`].
    ///
    /// Use this when the test needs to pre-populate the provider with custom
    /// [`SystemConfig`] entries before derivation starts.
    ///
    /// [`SystemConfig`]: base_consensus_genesis::SystemConfig
    pub fn create_verifier_with_l2_provider(
        &self,
        l2_provider: ActionL2ChainProvider,
    ) -> (L2Verifier, SharedL1Chain) {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        let rollup_config = Arc::new(self.rollup_config.clone());
        let l1_chain_config = Arc::new(L1ChainConfig::default());

        let l1_provider = ActionL1ChainProvider::new(chain.clone());
        let dap_source =
            ActionDataSource::new(chain.clone(), self.rollup_config.batch_inbox_address);

        let genesis_l1_block = self.l1.chain().first().expect("genesis always present");
        let genesis_l1 = block_info_from(genesis_l1_block);

        let safe_head = L2BlockInfo {
            block_info: BlockInfo {
                hash: self.rollup_config.genesis.l2.hash,
                number: self.rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: self.rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash { number: genesis_l1.number, hash: genesis_l1.hash },
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
}

impl Default for ActionTestHarness {
    fn default() -> Self {
        Self::new(L1MinerConfig::default(), RollupConfig::default())
    }
}
