use std::{fmt::Debug, sync::Arc};

use alloy_eips::BlockNumHash;
use alloy_genesis::ChainConfig;
use base_common_consensus::{BaseBlock, BaseTxEnvelope};
use base_consensus_derive::{DataAvailabilityProvider, PipelineBuilder, StatefulAttributesBuilder};
use base_consensus_genesis::RollupConfig;
use base_consensus_node::L1OriginSelector;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};

use crate::{
    ActionBlobDataSource, ActionDataSource, ActionEngineClient, ActionL1ChainProvider,
    ActionL2ChainProvider, ActionL2Source, ActionPipeline, BlobVerifierPipeline, L1Miner,
    L1MinerConfig, L2Sequencer, SharedL1Chain, TestGossipTransport, TestRollupNode,
    VerifierPipeline, block_info_from,
};

/// Top-level test harness that owns all actors for a single action test.
///
/// `ActionTestHarness` is the entry point for writing action tests. It holds
/// the [`L1Miner`] and the [`RollupConfig`] shared by all actors. Tests drive
/// the harness step-by-step using the public actor APIs.
///
/// L2 blocks are produced by an [`L2Sequencer`] obtained via
/// [`create_l2_sequencer`]. Blocks contain real L1-info deposit transactions
/// and real signed EIP-1559 user transactions -- no simplified mock types.
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
    ///
    /// Sets `rollup_config.genesis.l2.hash` to the real Reth genesis block hash so
    /// the pipeline's `l2_safe_head.hash` matches the `parent_hash` encoded in batches.
    /// Without this, `check_batch` drops every first batch with `ParentHashMismatch`
    /// because `build_and_commit` substitutes `B256::ZERO` with the real genesis hash.
    pub fn new(l1_config: L1MinerConfig, mut rollup_config: RollupConfig) -> Self {
        rollup_config.genesis.l2.hash = ActionEngineClient::compute_l2_genesis_hash(&rollup_config);
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
        let genesis_l1_number = self.rollup_config.genesis.l1.number;
        let genesis_l1 =
            self.l1.block_by_number(genesis_l1_number).map(block_info_from).unwrap_or_else(|| {
                block_info_from(self.l1.chain().first().expect("genesis always present"))
            });
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

    /// Create a [`SupervisedP2P`] / [`TestGossipTransport`] channel pair and
    /// wire the handle to `sequencer`.
    ///
    /// After this call, [`L2Sequencer::broadcast_unsafe_block`] delivers blocks
    /// into the returned [`TestGossipTransport`]. The transport can be held by
    /// a `TestRollupNode` or polled directly in single-node tests.
    ///
    /// [`SupervisedP2P`]: crate::SupervisedP2P
    pub fn create_supervised_p2p(&self, sequencer: &mut L2Sequencer) -> TestGossipTransport {
        let (p2p, transport) = TestGossipTransport::channel();
        sequencer.set_supervised_p2p(p2p);
        transport
    }

    /// Create a [`TestRollupNode`] wired to a sequencer's block-hash registry.
    ///
    /// Builds the full derivation pipeline and an [`ActionEngineClient`] that
    /// shares `sequencer.block_hash_registry()`, ensuring state-root
    /// comparisons in `new_payload_vX` work automatically. The `l1_chain` is
    /// shared between the providers and the engine client so newly pushed L1
    /// blocks are visible to both.
    ///
    /// The returned node must be [`initialize`]d before its first [`step`] or
    /// [`run_until_idle`] call:
    ///
    /// ```rust,ignore
    /// let mut node = h.create_test_rollup_node(&sequencer, chain, transport);
    /// node.initialize().await;
    /// ```
    ///
    /// [`initialize`]: TestRollupNode::initialize
    /// [`step`]: TestRollupNode::step
    /// [`run_until_idle`]: TestRollupNode::run_until_idle
    pub fn create_test_rollup_node(
        &self,
        sequencer: &L2Sequencer,
        l1_chain: SharedL1Chain,
        p2p: TestGossipTransport,
    ) -> TestRollupNode<VerifierPipeline> {
        let dap_source =
            ActionDataSource::new(l1_chain.clone(), self.rollup_config.batch_inbox_address);
        self.build_node_inner(sequencer, l1_chain, p2p, dap_source)
    }

    /// Create a [`TestRollupNode`] wired to blob DA.
    ///
    /// Identical to [`create_test_rollup_node`] but uses
    /// [`ActionBlobDataSource`] so the pipeline reads blobs from the L1 chain
    /// instead of calldata.
    ///
    /// [`create_test_rollup_node`]: ActionTestHarness::create_test_rollup_node
    pub fn create_blob_test_rollup_node(
        &self,
        sequencer: &L2Sequencer,
        l1_chain: SharedL1Chain,
        p2p: TestGossipTransport,
    ) -> TestRollupNode<BlobVerifierPipeline> {
        let dap_source =
            ActionBlobDataSource::new(l1_chain.clone(), self.rollup_config.batch_inbox_address);
        self.build_node_inner(sequencer, l1_chain, p2p, dap_source)
    }

    /// Build a [`TestRollupNode`] for any data-availability source.
    ///
    /// Shared implementation for [`create_test_rollup_node`] and
    /// [`create_blob_test_rollup_node`]; the two public methods differ only in
    /// which `dap_source` they construct before delegating here.
    ///
    /// [`create_test_rollup_node`]: ActionTestHarness::create_test_rollup_node
    /// [`create_blob_test_rollup_node`]: ActionTestHarness::create_blob_test_rollup_node
    fn build_node_inner<D>(
        &self,
        sequencer: &L2Sequencer,
        l1_chain: SharedL1Chain,
        p2p: TestGossipTransport,
        dap_source: D,
    ) -> TestRollupNode<ActionPipeline<D>>
    where
        D: DataAvailabilityProvider + Send + Sync + Debug,
    {
        let rollup_config = Arc::new(self.rollup_config.clone());
        let l1_chain_config = Arc::new(ChainConfig::default());

        let l1_provider = ActionL1ChainProvider::new(l1_chain.clone());
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);

        let genesis_l1 = block_info_from(self.l1.chain().first().expect("genesis always present"));
        let safe_head = self.l2_genesis();

        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            Arc::clone(&l1_chain_config),
            l2_provider.clone(),
            l1_provider.clone(),
        );
        let pipeline = PipelineBuilder::new()
            .rollup_config(Arc::clone(&rollup_config))
            .origin(genesis_l1)
            .chain_provider(l1_provider)
            .dap_source(dap_source)
            .l2_chain_provider(l2_provider)
            .builder(attrs_builder)
            .build_polled();

        // Create an independent engine client for the derivation node. The node uses
        // `execute_from_attrs` which passes the full `BasePayloadAttributes` (including
        // Holocene/Jovian parameters), so it can build any block from scratch without
        // needing the sequencer's pre-built headers.
        //
        // Share the sequencer's block-hash registry so that state-root comparisons in
        // `execute_from_attrs` work: when the sequencer pre-builds a block its state root
        // is registered, and the derivation node asserts the re-derived root matches.
        let engine = ActionEngineClient::new(
            Arc::clone(&rollup_config),
            safe_head,
            sequencer.block_hash_registry(),
            l1_chain,
        );

        TestRollupNode::new(pipeline, engine, p2p, safe_head, rollup_config)
    }

    /// Create a [`TestRollupNode`] wired to a sequencer's block-hash registry, returning
    /// `(node, l1_chain)` for convenient L1-push-then-signal patterns.
    ///
    /// Wires `sequencer` to a fresh [`TestGossipTransport`] channel and builds the
    /// full calldata derivation pipeline.
    pub fn create_test_rollup_node_from_sequencer(
        &self,
        sequencer: &mut L2Sequencer,
        l1_chain: SharedL1Chain,
    ) -> (TestRollupNode<VerifierPipeline>, SharedL1Chain) {
        let transport = self.create_supervised_p2p(sequencer);
        let node = self.create_test_rollup_node(sequencer, l1_chain.clone(), transport);
        (node, l1_chain)
    }

    /// Create a blob-DA [`TestRollupNode`] wired to a sequencer's block-hash registry,
    /// returning `(node, l1_chain)`.
    ///
    /// Identical to [`create_test_rollup_node_from_sequencer`] but uses blob DA.
    ///
    /// [`create_test_rollup_node_from_sequencer`]: ActionTestHarness::create_test_rollup_node_from_sequencer
    pub fn create_blob_test_rollup_node_from_sequencer(
        &self,
        sequencer: &mut L2Sequencer,
        l1_chain: SharedL1Chain,
    ) -> (TestRollupNode<BlobVerifierPipeline>, SharedL1Chain) {
        let transport = self.create_supervised_p2p(sequencer);
        let node = self.create_blob_test_rollup_node(sequencer, l1_chain.clone(), transport);
        (node, l1_chain)
    }

    /// Create an [`L2Sequencer`] starting from L2 genesis, wired to a
    /// snapshot of the current L1 chain.
    ///
    /// The returned sequencer generates real [`BaseBlock`]s using the production
    /// [`L1OriginSelector`], [`StatefulAttributesBuilder`], and
    /// [`ActionEngineClient`] (backed by `BasePayloadBuilder`).
    ///
    /// Call `build_next_block_with_single_transaction().await` once per L2
    /// block to advance the sequencer.
    ///
    /// After mining new L1 blocks, push them to the [`SharedL1Chain`] returned
    /// alongside the verifier so the sequencer sees the updated epochs.
    pub fn create_l2_sequencer(&self, l1_chain: SharedL1Chain) -> L2Sequencer {
        let rollup_config = Arc::new(self.rollup_config.clone());
        let l1_chain_config = Arc::new(ChainConfig::default());

        let genesis_head = self.l2_genesis();

        let l1_provider = ActionL1ChainProvider::new(l1_chain.clone());
        let l2_provider = ActionL2ChainProvider::from_genesis(&self.rollup_config);

        let attrs_builder = StatefulAttributesBuilder::new(
            Arc::clone(&rollup_config),
            Arc::clone(&l1_chain_config),
            l2_provider.clone(),
            l1_provider,
        );

        let origin_selector = L1OriginSelector::new(Arc::clone(&rollup_config), l1_chain.clone());

        let engine_client = Arc::new(ActionEngineClient::new(
            Arc::clone(&rollup_config),
            genesis_head,
            crate::SharedBlockHashRegistry::new(),
            l1_chain,
        ));

        L2Sequencer::new(
            genesis_head,
            origin_selector,
            attrs_builder,
            engine_client,
            rollup_config,
            l2_provider,
        )
    }

    /// Decode the [`L1BlockInfoTx`] from the first deposit transaction of an
    /// [`BaseBlock`].
    ///
    /// Every L2 block begins with an L1 info deposit whose calldata encodes the
    /// active [`L1BlockInfoTx`] variant (Bedrock / Ecotone / Isthmus / Jovian).
    /// Use this to assert that the correct format is used at hardfork boundaries.
    ///
    /// # Panics
    ///
    /// Panics if the first transaction is not a deposit or if the calldata
    /// cannot be decoded.
    pub fn l1_info_from_block(block: &BaseBlock) -> L1BlockInfoTx {
        let BaseTxEnvelope::Deposit(sealed) = &block.body.transactions[0] else {
            panic!("first transaction must be a deposit");
        };
        L1BlockInfoTx::decode_calldata(sealed.inner().input.as_ref())
            .expect("L1 info calldata must decode")
    }

    /// Build an [`ActionL2Source`] pre-populated with `n` real [`BaseBlock`]s
    /// starting from L2 genesis.
    ///
    /// Use this when a test needs a ready-made block source and does not
    /// require direct access to the underlying [`L2Sequencer`].
    ///
    /// Note: this is an async operation because the sequencer now uses the
    /// production engine. If you need a sync source builder, construct the
    /// sequencer manually and drive it with an async runtime.
    ///
    /// [`BaseBlock`]: base_common_consensus::BaseBlock
    pub async fn create_l2_source(&self, n: u64) -> ActionL2Source {
        let chain = SharedL1Chain::from_blocks(self.l1.chain().to_vec());
        let mut sequencer = self.create_l2_sequencer(chain);
        let mut source = ActionL2Source::new();
        for _ in 0..n {
            source.push(sequencer.build_next_block_with_single_transaction().await);
        }
        source
    }
}

impl Default for ActionTestHarness {
    fn default() -> Self {
        Self::new(L1MinerConfig::default(), RollupConfig::default())
    }
}
