//! op-rbuilder Node types config.
//!
//! Inherits Network, Executor, and Consensus Builders from the optimism node,
//! and overrides the Pool and Payload Builders.

use alloy_consensus::Header;
use rbuilder::live_builder::config::Config;
use rbuilder_bundle_pool_operations::BundlePoolOps;
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_evm::ConfigureEvm;
use reth_node_api::NodePrimitives;
use reth_node_builder::{
    components::{ComponentsBuilder, PayloadServiceBuilder, PoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder, NodeTypesWithEngine,
    PayloadBuilderConfig,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::OpEvmConfig;
use reth_optimism_node::{
    node::{OpAddOns, OpConsensusBuilder, OpExecutorBuilder, OpNetworkBuilder, OpPrimitives},
    txpool::OpTransactionValidator,
    OpEngineTypes,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_primitives::TransactionSigned;
use reth_provider::{BlockReader, CanonStateSubscriptions, DatabaseProviderFactory};
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    TransactionValidationTaskExecutor,
};
use reth_trie_db::MerklePatriciaTrie;
use std::sync::Arc;
use transaction_pool_bundle_ext::{
    BundlePoolOperations, BundleSupportedPool, TransactionPoolBundleExt,
};

use crate::args::OpRbuilderArgs;

/// Optimism primitive types.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpRbuilderPrimitives;

impl NodePrimitives for OpRbuilderPrimitives {
    type Block = reth_primitives::Block;
    type SignedTx = reth_primitives::TransactionSigned;
    type TxType = reth_primitives::TxType;
    type Receipt = reth_primitives::Receipt;
}

/// Type configuration for an Optimism rbuilder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderNode {
    /// Additional args
    pub args: OpRbuilderArgs,
    /// rbuilder config
    pub config: Config,
}

impl OpRbuilderNode {
    /// Creates a new instance of the OP rbuilder node type.
    pub const fn new(args: OpRbuilderArgs, config: Config) -> Self {
        Self { args, config }
    }

    /// Returns the components for the given [`OpRbuilderArgs`].
    pub fn components<Node>(
        args: OpRbuilderArgs,
        config: Config,
    ) -> ComponentsBuilder<
        Node,
        OpRbuilderPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>,
        >,
        <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
    {
        let OpRbuilderArgs {
            disable_txpool_gossip,
            compute_pending_block,
            discovery_v4,
            add_builder_tx,
            ..
        } = args;
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(OpRbuilderPoolBuilder::new(config.clone()))
            .payload(OpRbuilderPayloadServiceBuilder::new(
                compute_pending_block,
                add_builder_tx,
                config.clone(),
            ))
            .network(OpNetworkBuilder {
                disable_txpool_gossip,
                disable_discovery_v4: !discovery_v4,
            })
            .executor(OpExecutorBuilder::default())
            .consensus(OpConsensusBuilder::default())
    }
}

impl<N> Node<N> for OpRbuilderNode
where
    N: FullNodeTypes<Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>>,
    <<N as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpRbuilderPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns =
        OpAddOns<NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let Self { args, config } = self;
        Self::components(args.clone(), config.clone())
    }

    fn add_ons(&self) -> Self::AddOns {
        OpAddOns::new(self.args.sequencer_http.clone())
    }
}

impl NodeTypes for OpRbuilderNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type StateCommitment = MerklePatriciaTrie;
}

impl NodeTypesWithEngine for OpRbuilderNode {
    type Engine = OpEngineTypes;
}

/// An extended optimism transaction pool with bundle support.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderPoolBuilder {
    config: Config,
}

impl OpRbuilderPoolBuilder {
    /// Creates a new instance of the OP rbuilder pool builder.
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

pub type OpRbuilderTransactionPool<Client, S> = BundleSupportedPool<
    TransactionValidationTaskExecutor<OpTransactionValidator<Client, EthPooledTransaction>>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    S,
    BundlePoolOps,
>;

impl<Node> PoolBuilder<Node> for OpRbuilderPoolBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec>>,
    <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
{
    type Pool = OpRbuilderTransactionPool<Node::Provider, DiskFileBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;

        let validator = TransactionValidationTaskExecutor::eth_builder(Arc::new(
            ctx.chain_spec().inner.clone(),
        ))
        .with_head_timestamp(ctx.head().timestamp)
        .kzg_settings(ctx.kzg_settings()?)
        .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
        .build_with_tasks(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            blob_store.clone(),
        )
        .map(|validator| {
            OpTransactionValidator::new(validator)
                // In --dev mode we can't require gas fees because we're unable to decode the L1
                // block info
                .require_l1_data_gas_fee(!ctx.config().dev.dev)
        });

        let bundle_ops = BundlePoolOps::new(ctx.provider().clone(), self.config)
            .await
            .expect("Failed to instantiate RbuilderBundlePoolOps");
        let transaction_pool = OpRbuilderTransactionPool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            bundle_ops,
            ctx.pool_config(),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        let transactions_path = data_dir.txpool_transactions();

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            let transactions_backup_config =
                reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

            ctx.task_executor()
                .spawn_critical_with_graceful_shutdown_signal(
                    "local transactions backup task",
                    |shutdown| {
                        reth_transaction_pool::maintain::backup_local_transactions_task(
                            shutdown,
                            pool.clone(),
                            transactions_backup_config,
                        )
                    },
                );

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool,
                    chain_events,
                    ctx.task_executor().clone(),
                    Default::default(),
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

/// An OP rbuilder payload service builder.
#[derive(Debug, Default, Clone)]
pub struct OpRbuilderPayloadServiceBuilder {
    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    pub compute_pending_block: bool,
    /// Whether to add a builder tx to the end of block.
    /// This is used to verify blocks landed onchain was
    /// built by the builder
    pub add_builder_tx: bool,
    /// rbuilder config to get coinbase signer
    pub config: Config,
}

impl OpRbuilderPayloadServiceBuilder {
    /// Create a new instance with the given `compute_pending_block` flag.
    pub const fn new(compute_pending_block: bool, add_builder_tx: bool, config: Config) -> Self {
        Self {
            compute_pending_block,
            add_builder_tx,
            config,
        }
    }

    /// A helper method to initialize [`PayloadBuilderService`] with the given EVM config.
    pub fn spawn<Node, Evm, Pool>(
        self,
        evm_config: Evm,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<OpEngineTypes>>
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>,
        >,
        <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
        Pool: TransactionPoolBundleExt
            + BundlePoolOperations<Transaction = TransactionSigned>
            + Unpin
            + 'static,

        Evm: ConfigureEvm<Header = Header>,
    {
        let builder_signer = if self.add_builder_tx {
            Some(self.config.base_config.coinbase_signer()?)
        } else {
            None
        };
        let payload_builder =
            op_rbuilder_payload_builder::OpRbuilderPayloadBuilder::new(evm_config)
                .set_compute_pending_block(self.compute_pending_block)
                .set_builder_signer(builder_signer);
        let conf = ctx.payload_builder_config();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval())
            .deadline(conf.deadline())
            .max_payload_tasks(conf.max_payload_tasks())
            // no extradata for OP
            .extradata(Default::default());

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for OpRbuilderPayloadServiceBuilder
where
    Node:
        FullNodeTypes<Types: NodeTypesWithEngine<Engine = OpEngineTypes, ChainSpec = OpChainSpec>>,
    <<Node as FullNodeTypes>::Provider as DatabaseProviderFactory>::Provider: BlockReader,
    Pool: TransactionPoolBundleExt
        + BundlePoolOperations<Transaction = TransactionSigned>
        + Unpin
        + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<OpEngineTypes>> {
        self.spawn(OpEvmConfig::new(ctx.chain_spec()), ctx, pool)
    }
}
