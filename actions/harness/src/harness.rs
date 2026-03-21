use std::sync::Arc;

use alloy_eips::BlockNumHash;
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::{L1ChainConfig, RollupConfig};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};

use crate::{
    ActionBlobDataSource, ActionDataSource, ActionL1ChainProvider, ActionL2ChainProvider,
    ActionL2Source, BlobVerifierPipeline, L1Miner, L1MinerConfig, L2Sequencer, L2Verifier,
    SharedL1Chain, StatefulL2Executor, VerifierPipeline, block_info_from,
};

/// Top-level test harness that owns all actors for a single action test.
///
/// `ActionTestHarness` is the entry point for writing action tests. It holds
/// the [`L1Miner`] and the [`RollupConfig`] shared by all actors. Tests drive
/// the harness step-by-step using the public actor APIs.
///
/// L2 blocks are produced by an [`L2Sequencer`] obtained via
/// [`create_l2_sequencer`]. Blocks contain real L1-info deposit transactions
/// and real signed EIP-1559 user transactions — no simplified mock types.
///
/// [`create_l2_sequencer`]: ActionTestHarness::create_l2_sequencer
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

    /// Mine one L1 block and immediately push it to the given shared chain.
    ///
    /// Equivalent to calling `self.l1.mine_block()` followed by
    /// `chain.push(self.l1.tip().clone())`. Returns the [`BlockInfo`] of the
    /// newly mined block for use in pipeline signals.
    pub fn mine_and_push(&mut self, chain: &SharedL1Chain) -> BlockInfo {
        self.l1.mine_block();
        chain.push(self.l1.tip().clone());
        block_info_from(self.l1.tip())
    }

    /// Return the L2 genesis [`L2BlockInfo`] anchored to the L1 genesis block.
    ///
    /// Convenience method eliminating the repeated 10-line construction used in
    /// reorg reset tests.
    pub fn l2_genesis(&self) -> L2BlockInfo {
        let genesis_l1 = block_info_from(self.l1.chain().first().expect("genesis always present"));
        L2BlockInfo {
            block_info: BlockInfo {
                hash: self.rollup_config.genesis.l2.hash,
                number: self.rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: self.rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash { number: genesis_l1.number, hash: genesis_l1.hash },
            seq_num: 0,
        }
    }

    /// Create an [`L2Sequencer`] starting from L2 genesis, wired to a
    /// snapshot of the current L1 chain.
    ///
    /// The returned sequencer generates real [`OpBlock`]s with a proper
    /// L1-info deposit transaction (first tx) and signed EIP-1559 user
    /// transactions. Call `build_next_block_with_single_transaction()` once per L2 block to advance
    /// the sequencer.
    ///
    /// After mining new L1 blocks, push them to the [`SharedL1Chain`] returned
    /// alongside the verifier so the sequencer sees the updated epochs.
    pub fn create_l2_sequencer(&self, l1_chain: SharedL1Chain) -> L2Sequencer {
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

        L2Sequencer::new(genesis_head, l1_chain, self.rollup_config.clone(), system_config)
    }

    /// Decode the [`L1BlockInfoTx`] from the first deposit transaction of an
    /// [`OpBlock`].
    ///
    /// Every L2 block begins with an L1 info deposit whose calldata encodes the
    /// active [`L1BlockInfoTx`] variant (Bedrock / Ecotone / Isthmus / Jovian).
    /// Use this to assert that the correct format is used at hardfork boundaries.
    ///
    /// # Panics
    ///
    /// Panics if the first transaction is not a deposit or if the calldata
    /// cannot be decoded.
    pub fn l1_info_from_block(block: &OpBlock) -> L1BlockInfoTx {
        let OpTxEnvelope::Deposit(sealed) = &block.body.transactions[0] else {
            panic!("first transaction must be a deposit");
        };
        L1BlockInfoTx::decode_calldata(sealed.inner().input.as_ref())
            .expect("L1 info calldata must decode")
    }

    /// Build an [`ActionL2Source`] pre-populated with `n` real [`OpBlock`]s
    /// starting from L2 genesis.
    ///
    /// Use this when a test needs a ready-made block source and does not
    /// require direct access to the underlying [`L2Sequencer`].
    ///
    /// [`OpBlock`]: base_alloy_consensus::OpBlock
    pub fn create_l2_source(&self, n: u64) -> ActionL2Source {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        let mut sequencer = self.create_l2_sequencer(chain);
        let mut source = ActionL2Source::new();
        for _ in 0..n {
            source.push(sequencer.build_next_block_with_single_transaction());
        }
        source
    }

    /// Create an [`L2Verifier`] wired to the harness's L1 chain.
    ///
    /// A [`SharedL1Chain`] is initialised from the miner's current chain and
    /// returned alongside the verifier. Mine new blocks with `l1.mine_block()`
    /// then call `chain.push(l1.tip().clone())` and
    /// `verifier.act_l1_head_signal(block_info).await` to feed them into the
    /// pipeline.
    pub fn create_verifier(&self) -> (L2Verifier<VerifierPipeline>, SharedL1Chain) {
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);
        self.create_verifier_with_l2_provider(l2_provider)
    }

    /// Create an [`L2Verifier`] wired to a sequencer's block-hash registry and
    /// a fresh [`StatefulL2Executor`] for per-block state-root validation.
    ///
    /// This is the normal path for tests that build blocks with [`L2Sequencer`]
    /// and then derive them with a verifier. The supplied `l1_chain` becomes
    /// the verifier's shared L1 view and is returned so tests can keep pushing
    /// newly mined L1 blocks into it.
    ///
    /// After every derived [`OpAttributesWithParent`], the verifier re-executes
    /// the block's transactions through the EVM and asserts the computed state
    /// root matches the value registered by [`L2Sequencer`]. Validation is
    /// silently skipped for blocks whose state root was not registered (e.g.
    /// deposit-only blocks produced by the pipeline without sequencer input).
    pub fn create_verifier_from_sequencer(
        &self,
        sequencer: &L2Sequencer,
        l1_chain: SharedL1Chain,
    ) -> (L2Verifier<VerifierPipeline>, SharedL1Chain) {
        let executor = StatefulL2Executor::new(self.rollup_config.clone());
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);
        let (verifier, chain) =
            self.create_verifier_with_l2_provider_and_chain(l2_provider, l1_chain);
        (
            verifier
                .with_block_hash_registry(sequencer.block_hash_registry())
                .with_executor(executor),
            chain,
        )
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
    ) -> (L2Verifier<VerifierPipeline>, SharedL1Chain) {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        self.create_verifier_with_l2_provider_and_chain(l2_provider, chain)
    }

    /// Create an [`L2Verifier`] wired to blob DA.
    ///
    /// Identical to [`create_verifier`] but uses [`ActionBlobDataSource`] so
    /// the pipeline reads blobs from the L1 chain instead of calldata.
    ///
    /// [`create_verifier`]: ActionTestHarness::create_verifier
    pub fn create_blob_verifier(&self) -> (L2Verifier<BlobVerifierPipeline>, SharedL1Chain) {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        self.create_blob_verifier_with_chain(chain)
    }

    /// Create a blob verifier wired to a sequencer's block-hash registry and
    /// a fresh [`StatefulL2Executor`] for per-block state-root validation.
    ///
    /// Identical to [`create_verifier_from_sequencer`] but uses blob DA.
    ///
    /// [`create_verifier_from_sequencer`]: ActionTestHarness::create_verifier_from_sequencer
    pub fn create_blob_verifier_from_sequencer(
        &self,
        sequencer: &L2Sequencer,
        l1_chain: SharedL1Chain,
    ) -> (L2Verifier<BlobVerifierPipeline>, SharedL1Chain) {
        let executor = StatefulL2Executor::new(self.rollup_config.clone());
        let (verifier, chain) = self.create_blob_verifier_with_chain(l1_chain);
        (
            verifier
                .with_block_hash_registry(sequencer.block_hash_registry())
                .with_executor(executor),
            chain,
        )
    }

    fn create_verifier_with_l2_provider_and_chain(
        &self,
        l2_provider: ActionL2ChainProvider,
        chain: SharedL1Chain,
    ) -> (L2Verifier<VerifierPipeline>, SharedL1Chain) {
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

        (
            L2Verifier::new(
                rollup_config,
                l1_chain_config,
                l1_provider,
                dap_source,
                l2_provider,
                safe_head,
                genesis_l1,
            ),
            chain,
        )
    }

    fn create_blob_verifier_with_chain(
        &self,
        chain: SharedL1Chain,
    ) -> (L2Verifier<BlobVerifierPipeline>, SharedL1Chain) {
        let rollup_config = Arc::new(self.rollup_config.clone());
        let l1_chain_config = Arc::new(L1ChainConfig::default());

        let l1_provider = ActionL1ChainProvider::new(chain.clone());
        let dap_source =
            ActionBlobDataSource::new(chain.clone(), self.rollup_config.batch_inbox_address);

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

        (
            L2Verifier::new_blob(
                rollup_config,
                l1_chain_config,
                l1_provider,
                dap_source,
                ActionL2ChainProvider::from_genesis(&self.rollup_config),
                safe_head,
                genesis_l1,
            ),
            chain,
        )
    }
}

impl Default for ActionTestHarness {
    fn default() -> Self {
        Self::new(L1MinerConfig::default(), RollupConfig::default())
    }
}
