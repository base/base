//! op-rbuilder Node types config.
//!
//! Inherits Network, Executor, and Consensus Builders from the optimism node,
//! and overrides the Pool and Payload Builders.

use rbuilder_bundle_pool_operations::BundlePoolOps;
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_chainspec::ChainSpec;
use reth_evm::ConfigureEvm;
use reth_evm_optimism::OptimismEvmConfig;
use reth_node_builder::{
    components::{ComponentsBuilder, PayloadServiceBuilder, PoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, Node, PayloadBuilderConfig,
};
use reth_node_optimism::{
    node::{
        OptimismAddOns, OptimismConsensusBuilder, OptimismExecutorBuilder, OptimismNetworkBuilder,
    },
    txpool::OpTransactionValidator,
    OptimismEngineTypes,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_primitives::TransactionSigned;
use reth_provider::CanonStateSubscriptions;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPooledTransaction,
    TransactionValidationTaskExecutor,
};
use std::path::PathBuf;
use transaction_pool_bundle_ext::{
    BundlePoolOperations, BundleSupportedPool, TransactionPoolBundleExt,
};

use crate::args::OpRbuilderArgs;

/// Type configuration for an OP rbuilder node.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderNode {
    /// Additional args
    pub args: OpRbuilderArgs,
}

impl OpRbuilderNode {
    /// Creates a new instance of the OP rbuilder node type.
    pub const fn new(args: OpRbuilderArgs) -> Self {
        Self { args }
    }

    /// Returns the components for the given [`OpRbuilderArgs`].
    pub fn components<Node>(
        args: OpRbuilderArgs,
    ) -> ComponentsBuilder<
        Node,
        OpRbuilderPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OptimismNetworkBuilder,
        OptimismExecutorBuilder,
        OptimismConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Engine = OptimismEngineTypes, ChainSpec = ChainSpec>,
    {
        let OpRbuilderArgs {
            disable_txpool_gossip,
            compute_pending_block,
            discovery_v4,
            rbuilder_config_path,
            ..
        } = args;
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(OpRbuilderPoolBuilder::new(rbuilder_config_path))
            .payload(OpRbuilderPayloadServiceBuilder::new(
                compute_pending_block,
                OptimismEvmConfig::default(),
            ))
            .network(OptimismNetworkBuilder {
                disable_txpool_gossip,
                disable_discovery_v4: !discovery_v4,
            })
            .executor(OptimismExecutorBuilder::default())
            .consensus(OptimismConsensusBuilder::default())
    }
}

impl<N> Node<N> for OpRbuilderNode
where
    N: FullNodeTypes<Engine = OptimismEngineTypes, ChainSpec = ChainSpec>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpRbuilderPoolBuilder,
        OpRbuilderPayloadServiceBuilder,
        OptimismNetworkBuilder,
        OptimismExecutorBuilder,
        OptimismConsensusBuilder,
    >;

    type AddOns = OptimismAddOns;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let Self { args } = self;
        Self::components(args.clone())
    }
}

impl NodeTypes for OpRbuilderNode {
    type Primitives = ();
    type Engine = OptimismEngineTypes;
    type ChainSpec = ChainSpec;
}

/// An extended optimism transaction pool with bundle support.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpRbuilderPoolBuilder {
    rbuilder_config_path: PathBuf,
}

impl OpRbuilderPoolBuilder {
    /// Creates a new instance of the OP rbuilder pool builder.
    pub fn new(rbuilder_config_path: PathBuf) -> Self {
        Self {
            rbuilder_config_path,
        }
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
    Node: FullNodeTypes,
{
    type Pool = OpRbuilderTransactionPool<Node::Provider, DiskFileBlobStore>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;

        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.chain_spec())
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

        let bundle_ops = BundlePoolOps::new(ctx.provider().clone(), self.rbuilder_config_path)
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

/// A op-rbuilder payload service builder
#[derive(Debug, Default, Clone)]
pub struct OpRbuilderPayloadServiceBuilder<EVM = OptimismEvmConfig> {
    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    pub compute_pending_block: bool,
    /// The EVM configuration to use for the payload builder.
    pub evm_config: EVM,
}

impl<EVM> OpRbuilderPayloadServiceBuilder<EVM> {
    /// Create a new instance with the given `compute_pending_block` flag and evm config.
    pub const fn new(compute_pending_block: bool, evm_config: EVM) -> Self {
        Self {
            compute_pending_block,
            evm_config,
        }
    }
}

impl<Node, EVM, Pool> PayloadServiceBuilder<Node, Pool> for OpRbuilderPayloadServiceBuilder<EVM>
where
    Node: FullNodeTypes<Engine = OptimismEngineTypes, ChainSpec = ChainSpec>,
    Pool: TransactionPoolBundleExt
        + BundlePoolOperations<Transaction = TransactionSigned>
        + Unpin
        + 'static,
    EVM: ConfigureEvm,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<Node::Engine>> {
        let payload_builder = op_rbuilder_payload_builder::OpRbuilderPayloadBuilder::new(
            OptimismEvmConfig::default(),
        )
        .set_compute_pending_block(self.compute_pending_block);
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
            ctx.chain_spec(),
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}
