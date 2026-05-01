//! Base Node types config.

use std::{
    marker::PhantomData,
    net::{SocketAddrV4, SocketAddrV6},
    sync::Arc,
};

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B64, B256, Bytes, bytes::BytesMut};
use alloy_rlp::Encodable;
use base_common_chains::Upgrades;
use base_common_consensus::BasePrimitives;
use base_common_rpc_types_engine::{BasePayloadAttributes, ExecutionData};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::{BaseEvmConfig, BaseRethReceiptBuilder};
use base_execution_payload_builder::{
    Attributes, BaseBuiltPayload, PayloadPrimitives,
    builder::BasePayloadTransactions,
    config::{BaseBuilderConfig, BaseDAConfig, GasLimitConfig},
};
use base_execution_rpc::{
    MinerApiExtServer,
    config::{BaseEthConfigApiServer, BaseEthConfigHandler},
    eth::BaseEthApiBuilder,
    miner::BaseMinerExtApi,
    witness::BaseDebugWitnessApi,
};
use base_execution_storage::BaseStorage;
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, BasePooledTx, BaseTransactionPool,
    BaseTransactionValidator, TimestampedTransaction,
};
use reth_chainspec::{BaseFeeParams, ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5::enr::{IP_ENR_KEY, IP6_ENR_KEY};
use reth_evm::ConfigureEvm;
use reth_network::{
    NetworkConfig, NetworkHandle, NetworkManager, NetworkPrimitives, PeersInfo,
    types::BasicNetworkPrimitives,
};
use reth_node_api::{
    AddOnsContext, BuildNextEnv, EngineTypes, FullNodeComponents, HeaderTy, NodeAddOns,
    NodePrimitives, PayloadAttributesBuilder, PayloadTypes, PrimitivesTy, TxTy,
};
use reth_node_builder::{
    BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, ExecutorBuilder,
        NetworkBuilder, PayloadBuilderBuilder, PoolBuilder, PoolBuilderConfigOverrides,
        TxPoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        BasicEngineValidatorBuilder, EngineApiBuilder, EngineValidatorAddOn,
        EngineValidatorBuilder, EthApiBuilder, Identity, PayloadValidatorBuilder, RethRpcAddOns,
        RethRpcMiddleware, RethRpcServerHandles, RpcAddOns, RpcContext, RpcHandle,
    },
};
use reth_primitives_traits::{SealedHeader, header::HeaderMut};
use reth_provider::providers::ProviderFactoryBuilder;
use reth_rpc_api::{DebugApiServer, DebugExecutionWitnessApiServer, eth::RpcTypes};
use reth_rpc_server_types::RethRpcModule;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    EthPoolTransaction, PoolPooledTx, PoolTransaction, TransactionPool,
    TransactionValidationTaskExecutor, blobstore::DiskFileBlobStore,
};
use reth_trie_common::KeccakKeyHasher;
use serde::de::DeserializeOwned;

use crate::{
    BaseEngineApiBuilder, BaseEngineTypes,
    args::{RollupArgs, TxpoolOrdering},
    engine::BaseEngineValidator,
};

/// Discovery v5 protocol version for Base.
pub const BASE_V0_PROTOCOL_VERSION: [u8; 6] = *b"basev0";

/// Marker trait for Base node types with standard engine, chain spec, and primitives.
pub trait BaseNodeTypes:
    NodeTypes<Payload = BaseEngineTypes, ChainSpec = BaseChainSpec, Primitives = BasePrimitives>
{
}
/// Blanket impl for all node types that conform to the Base spec.
impl<N> BaseNodeTypes for N where
    N: NodeTypes<Payload = BaseEngineTypes, ChainSpec = BaseChainSpec, Primitives = BasePrimitives>
{
}

/// Helper trait for Base node types with full configuration including storage and execution
/// data.
pub trait BaseFullNodeTypes:
    NodeTypes<
        ChainSpec = BaseChainSpec,
        Primitives: PayloadPrimitives,
        Storage = BaseStorage,
        Payload: EngineTypes<ExecutionData = ExecutionData>,
    >
{
}

impl<N> BaseFullNodeTypes for N where
    N: NodeTypes<
            ChainSpec = BaseChainSpec,
            Primitives: PayloadPrimitives,
            Storage = BaseStorage,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >
{
}

/// Local payload attributes builder for Base.
///
/// This mirrors the upstream `LocalPayloadAttributesBuilder` for
/// `op_alloy_rpc_types_engine::BasePayloadAttributes`, but targets
/// `base_common_rpc_types_engine::BasePayloadAttributes`.
#[derive(Debug)]
pub struct BaseLocalPayloadAttributesBuilder {
    chain_spec: Arc<BaseChainSpec>,
}

impl BaseLocalPayloadAttributesBuilder {
    /// Creates a new builder.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl PayloadAttributesBuilder<BasePayloadAttributes> for BaseLocalPayloadAttributesBuilder {
    fn build(&self, parent: &SealedHeader<alloy_consensus::Header>) -> BasePayloadAttributes {
        /// Dummy system transaction for dev mode.
        const TX_SET_L1_BLOCK_BASE_MAINNET_BLOCK_1: [u8; 349] = alloy_primitives::hex!(
            "7ef90159a024fa2288af14732611c4b9a8f99b2c929eaf2af8fb45981a752a01417994df3b94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b90104015d8eb900000000000000000000000000000000000000000000000000000000010ac02800000000000000000000000000000000000000000000000000000000648a5ce300000000000000000000000000000000000000000000000000000003ded24b5e5c13d307623a926cd31415036c8b7fa14572f9dac64528e857a470511fc3077100000000000000000000000000000000000000000000000000000000000000010000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c900000000000000000000000000000000000000000000000000000000000000bc00000000000000000000000000000000000000000000000000000000000a6fe0"
        );

        let timestamp = std::cmp::max(
            parent.timestamp().saturating_add(1),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        );

        let default_eip_1559_params = BaseFeeParams::optimism();
        let denominator = std::env::var("BASE_DEV_EIP1559_DENOMINATOR")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_eip_1559_params.max_change_denominator as u32);
        let elasticity = std::env::var("BASE_DEV_EIP1559_ELASTICITY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_eip_1559_params.elasticity_multiplier as u32);
        let gas_limit =
            std::env::var("BASE_DEV_GAS_LIMIT").ok().and_then(|v| v.parse::<u64>().ok());

        let mut eip1559_bytes = [0u8; 8];
        eip1559_bytes[0..4].copy_from_slice(&denominator.to_be_bytes());
        eip1559_bytes[4..8].copy_from_slice(&elasticity.to_be_bytes());
        let eip_1559_params = Some(B64::from(eip1559_bytes));

        BasePayloadAttributes {
            payload_attributes: alloy_rpc_types_engine::PayloadAttributes {
                timestamp,
                prev_randao: B256::random(),
                suggested_fee_recipient: Address::random(),
                withdrawals: self
                    .chain_spec
                    .is_canyon_active_at_timestamp(timestamp)
                    .then(Default::default),
                parent_beacon_block_root: self
                    .chain_spec
                    .is_ecotone_active_at_timestamp(timestamp)
                    .then(B256::random),
            },
            transactions: Some(vec![TX_SET_L1_BLOCK_BASE_MAINNET_BLOCK_1.into()]),
            no_tx_pool: None,
            gas_limit,
            eip_1559_params,
            min_base_fee: Some(0),
        }
    }
}

/// Type configuration for a regular Base node.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BaseNode {
    /// Additional Base args
    pub args: RollupArgs,
    /// Data availability configuration for the OP builder.
    ///
    /// Used to throttle the size of the data availability payloads (configured by the batcher via
    /// the `miner_` api).
    ///
    /// By default no throttling is applied.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the OP builder.
    /// Used to control the gas limit of the blocks produced by the OP builder.(configured by the
    /// batcher via the `miner_` api)
    pub gas_limit_config: GasLimitConfig,
}

/// A [`ComponentsBuilder`] with its generic arguments set to a stack of Base-specific builders.
pub type BaseNodeComponentBuilder<Node, Payload = BasePayloadBuilder> = ComponentsBuilder<
    Node,
    BasePoolBuilder,
    BasicPayloadServiceBuilder<Payload>,
    BaseNetworkBuilder,
    BaseExecutorBuilder,
    BaseConsensusBuilder,
>;

impl BaseNode {
    /// Creates a new instance of the Base node type.
    pub fn new(args: RollupArgs) -> Self {
        Self {
            args,
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
        }
    }

    /// Configure the data availability configuration for the OP builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = da_config;
        self
    }

    /// Configure the gas limit configuration for the OP builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = gas_limit_config;
        self
    }

    /// Returns the components for the given [`RollupArgs`].
    pub fn components<Node>(&self) -> BaseNodeComponentBuilder<Node>
    where
        Node: FullNodeTypes<Types: BaseNodeTypes>,
    {
        let RollupArgs {
            disable_txpool_gossip,
            compute_pending_block,
            discovery_v4,
            txpool_ordering,
            base_protocol,
            ..
        } = self.args;
        let ordering = match txpool_ordering {
            TxpoolOrdering::CoinbaseTip => BaseOrdering::coinbase_tip(),
            TxpoolOrdering::Timestamp => BaseOrdering::timestamp(),
        };
        ComponentsBuilder::default()
            .node_types::<Node>()
            .executor(BaseExecutorBuilder::default())
            .pool(BasePoolBuilder::default().with_ordering(ordering))
            .payload(BasicPayloadServiceBuilder::new(
                BasePayloadBuilder::new(compute_pending_block)
                    .with_da_config(self.da_config.clone())
                    .with_gas_limit_config(self.gas_limit_config.clone()),
            ))
            .network(BaseNetworkBuilder::new(disable_txpool_gossip, !discovery_v4, base_protocol))
            .consensus(BaseConsensusBuilder::default())
    }

    /// Returns [`BaseAddOnsBuilder`] with configured arguments.
    pub fn add_ons_builder<NetworkT: RpcTypes>(&self) -> BaseAddOnsBuilder<NetworkT> {
        BaseAddOnsBuilder::default()
            .with_sequencer(self.args.sequencer.clone())
            .with_sequencer_headers(self.args.sequencer_headers.clone())
            .with_da_config(self.da_config.clone())
            .with_gas_limit_config(self.gas_limit_config.clone())
            .with_min_suggested_priority_fee(self.args.min_suggested_priority_fee)
    }

    /// Instantiates the [`ProviderFactoryBuilder`] for a Base node.
    ///
    /// # Open a Providerfactory in read-only mode from a datadir
    ///
    /// See also: [`ProviderFactoryBuilder`] and
    /// [`ReadOnlyConfig`](reth_provider::providers::ReadOnlyConfig).
    ///
    /// ```no_run
    /// use base_execution_chainspec::BASE_MAINNET;
    /// use base_node_core::BaseNode;
    ///
    /// fn demo(runtime: reth_tasks::Runtime) {
    ///     let factory = BaseNode::provider_factory_builder()
    ///         .open_read_only(BASE_MAINNET.clone(), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open a Providerfactory with custom config
    ///
    /// ```no_run
    /// use base_execution_chainspec::BaseChainSpecBuilder;
    /// use base_node_core::BaseNode;
    /// use reth_provider::providers::ReadOnlyConfig;
    ///
    /// fn demo(runtime: reth_tasks::Runtime) {
    ///     let factory = BaseNode::provider_factory_builder()
    ///         .open_read_only(
    ///             BaseChainSpecBuilder::base_mainnet().build().into(),
    ///             ReadOnlyConfig::from_datadir("datadir").no_watch(),
    ///             runtime,
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl<N> Node<N> for BaseNode
where
    N: FullNodeTypes<Types: BaseFullNodeTypes + BaseNodeTypes>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BasePoolBuilder,
        BasicPayloadServiceBuilder<BasePayloadBuilder>,
        BaseNetworkBuilder,
        BaseExecutorBuilder,
        BaseConsensusBuilder,
    >;

    type AddOns = BaseAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        BaseEthApiBuilder,
        BasePayloadValidatorBuilder,
        BaseEngineApiBuilder<BasePayloadValidatorBuilder>,
        BasicEngineValidatorBuilder<BasePayloadValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components(self)
    }

    fn add_ons(&self) -> Self::AddOns {
        self.add_ons_builder().build()
    }
}

impl<N> DebugNode<N> for BaseNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types_eth::Block<base_common_consensus::BaseTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        rpc_block.into_consensus()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        BaseLocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

impl NodeTypes for BaseNode {
    type Primitives = BasePrimitives;
    type ChainSpec = BaseChainSpec;
    type Storage = BaseStorage;
    type Payload = BaseEngineTypes;
}

/// Add-ons w.r.t. Base.
///
/// This type provides Base-specific addons to the node and exposes the RPC server and engine
/// API.
#[derive(Debug)]
pub struct BaseAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = BaseEngineApiBuilder<PVB>,
    EVB = BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
    /// Data availability configuration for the OP builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the OP builder.
    pub gas_limit_config: GasLimitConfig,
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    pub sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    pub sequencer_headers: Vec<String>,
    min_suggested_priority_fee: u64,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Creates a new instance from components.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        rpc_add_ons: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
        da_config: BaseDAConfig,
        gas_limit_config: GasLimitConfig,
        sequencer_url: Option<String>,
        sequencer_headers: Vec<String>,
        min_suggested_priority_fee: u64,
    ) -> Self {
        Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
        }
    }
}

impl<N> Default for BaseAddOns<N, BaseEthApiBuilder, BasePayloadValidatorBuilder>
where
    N: FullNodeComponents<Types: BaseNodeTypes>,
    BaseEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::builder().build()
    }
}

impl<N, NetworkT, RpcMiddleware>
    BaseAddOns<
        N,
        BaseEthApiBuilder<NetworkT>,
        BasePayloadValidatorBuilder,
        BaseEngineApiBuilder<BasePayloadValidatorBuilder>,
        RpcMiddleware,
    >
where
    N: FullNodeComponents<Types: BaseNodeTypes>,
    BaseEthApiBuilder<NetworkT>: EthApiBuilder<N>,
{
    /// Build a [`BaseAddOns`] using [`BaseAddOnsBuilder`].
    pub fn builder() -> BaseAddOnsBuilder<NetworkT> {
        BaseAddOnsBuilder::default()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Maps the [`reth_node_builder::rpc::EngineApiBuilder`] builder type.
    pub fn with_engine_api<T>(
        self,
        engine_api_builder: T,
    ) -> BaseAddOns<N, EthB, PVB, T, EVB, RpcMiddleware> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_engine_api(engine_api_builder),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
        )
    }

    /// Maps the [`PayloadValidatorBuilder`] builder type.
    pub fn with_payload_validator<T>(
        self,
        payload_validator_builder: T,
    ) -> BaseAddOns<N, EthB, T, EB, EVB, RpcMiddleware> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_payload_validator(payload_validator_builder),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
        )
    }

    /// Sets the RPC middleware stack for processing RPC requests.
    ///
    /// This method configures a custom middleware stack that will be applied to all RPC requests
    /// across HTTP, `WebSocket`, and IPC transports. The middleware is applied to the RPC service
    /// layer, allowing you to intercept, modify, or enhance RPC request processing.
    ///
    /// See also [`RpcAddOns::with_rpc_middleware`].
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOns<N, EthB, PVB, EB, EVB, T> {
        let Self {
            rpc_add_ons,
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
            ..
        } = self;
        BaseAddOns::new(
            rpc_add_ons.with_rpc_middleware(rpc_middleware),
            da_config,
            gas_limit_config,
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
        )
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, N, EthB::EthApi>, RethRpcServerHandles) -> eyre::Result<()>
            + Send
            + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.on_rpc_started(hook);
        self
    }

    /// Sets the hook that is run to configure the rpc modules.
    pub fn extend_rpc_modules<F>(mut self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, N, EthB::EthApi>) -> eyre::Result<()> + Send + 'static,
    {
        self.rpc_add_ons = self.rpc_add_ons.extend_rpc_modules(hook);
        self
    }
}

impl<N, EthB, PVB, EB, EVB, Attrs, RpcMiddleware> NodeAddOns<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: BaseNodeTypes
                       + NodeTypes<Payload: PayloadTypes<PayloadBuilderAttributes = Attrs>>,
            Evm: ConfigureEvm<
                NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<N::Types>, BaseChainSpec>,
            >,
            Pool: TransactionPool<Transaction: BasePooledTx>,
        >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
    Attrs: Attributes<Transaction = TxTy<N::Types>, RpcPayloadAttributes: DeserializeOwned>,
    <N::Types as NodeTypes>::Primitives: PayloadPrimitives<_Header: HeaderMut>,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        let Self { rpc_add_ons, da_config, gas_limit_config, .. } = self;
        let eth_config =
            BaseEthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        let builder = base_execution_payload_builder::BasePayloadBuilder::new(
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
        );
        // install additional OP specific rpc methods
        let debug_ext = BaseDebugWitnessApi::<_, _, _, Attrs>::new(
            ctx.node.provider().clone(),
            Box::new(ctx.node.task_executor().clone()),
            builder,
        );
        let miner_ext = BaseMinerExtApi::new(da_config, gas_limit_config);

        rpc_add_ons
            .launch_add_ons_with(ctx, move |container| {
                let reth_node_builder::rpc::RpcModuleContainer { modules, auth_module, registry } =
                    container;

                modules.merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;

                debug!(target: "reth::cli", "Installing debug payload witness rpc endpoint");
                modules.merge_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;

                // extend the miner namespace if configured in the regular http server
                modules.add_or_replace_if_module_configured(
                    RethRpcModule::Miner,
                    miner_ext.clone().into_rpc(),
                )?;

                // install the miner extension in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Miner) {
                    debug!(target: "reth::cli", "Installing miner DA rpc endpoint");
                    auth_module.merge_auth_methods(miner_ext.into_rpc())?;
                }

                // install the debug namespace in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Debug) {
                    debug!(target: "reth::cli", "Installing debug rpc endpoint");
                    auth_module.merge_auth_methods(registry.debug_api().into_rpc())?;
                }

                Ok(())
            })
            .await
    }
}

impl<N, EthB, PVB, EB, EVB, Attrs, RpcMiddleware> RethRpcAddOns<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<
            Types: BaseNodeTypes
                       + NodeTypes<Payload: PayloadTypes<PayloadBuilderAttributes = Attrs>>,
            Evm: ConfigureEvm<
                NextBlockEnvCtx: BuildNextEnv<Attrs, HeaderTy<N::Types>, BaseChainSpec>,
            >,
        >,
    <<N as FullNodeComponents>::Pool as TransactionPool>::Transaction: BasePooledTx,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
    Attrs: Attributes<Transaction = TxTy<N::Types>, RpcPayloadAttributes: DeserializeOwned>,
    <N::Types as NodeTypes>::Primitives: PayloadPrimitives<_Header: HeaderMut>,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.rpc_add_ons.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> EngineValidatorAddOn<N>
    for BaseAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: Send,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        EngineValidatorAddOn::engine_validator_builder(&self.rpc_add_ons)
    }
}

/// A regular Base EVM and executor builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BaseAddOnsBuilder<NetworkT, RpcMiddleware = Identity> {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Data availability configuration for the OP builder.
    da_config: Option<BaseDAConfig>,
    /// Gas limit configuration for the OP builder.
    gas_limit_config: Option<GasLimitConfig>,
    /// Marker for network types.
    _nt: PhantomData<NetworkT>,
    /// Minimum suggested priority fee (tip)
    min_suggested_priority_fee: u64,
    /// RPC middleware to use
    rpc_middleware: RpcMiddleware,
    /// Optional tokio runtime to use for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
}

impl<NetworkT> Default for BaseAddOnsBuilder<NetworkT> {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            da_config: None,
            gas_limit_config: None,
            min_suggested_priority_fee: 1_000_000,
            _nt: PhantomData,
            rpc_middleware: Identity::new(),
            tokio_runtime: None,
        }
    }
}

impl<NetworkT, RpcMiddleware> BaseAddOnsBuilder<NetworkT, RpcMiddleware> {
    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<String>) -> Self {
        self.sequencer_url = sequencer_client;
        self
    }

    /// With headers to use for the sequencer client requests.
    pub fn with_sequencer_headers(mut self, sequencer_headers: Vec<String>) -> Self {
        self.sequencer_headers = sequencer_headers;
        self
    }

    /// Configure the data availability configuration for the Base builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Configure the gas limit configuration for the Base payload builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = Some(gas_limit_config);
        self
    }

    /// Configure the minimum priority fee (tip)
    pub const fn with_min_suggested_priority_fee(mut self, min: u64) -> Self {
        self.min_suggested_priority_fee = min;
        self
    }

    /// Configures a custom tokio runtime for the RPC server.
    ///
    /// Caution: This runtime must not be created from within asynchronous context.
    pub fn with_tokio_runtime(mut self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        self.tokio_runtime = tokio_runtime;
        self
    }

    /// Configure the RPC middleware to use
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> BaseAddOnsBuilder<NetworkT, T> {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            tokio_runtime,
            _nt,
            ..
        } = self;
        BaseAddOnsBuilder {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            _nt,
            rpc_middleware,
            tokio_runtime,
        }
    }
}

impl<NetworkT, RpcMiddleware> BaseAddOnsBuilder<NetworkT, RpcMiddleware> {
    /// Builds an instance of [`BaseAddOns`].
    pub fn build<N, PVB, EB, EVB>(
        self,
    ) -> BaseAddOns<N, BaseEthApiBuilder<NetworkT>, PVB, EB, EVB, RpcMiddleware>
    where
        N: FullNodeComponents<Types: NodeTypes>,
        BaseEthApiBuilder<NetworkT>: EthApiBuilder<N>,
        PVB: PayloadValidatorBuilder<N> + Default,
        EB: Default,
        EVB: Default,
    {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            rpc_middleware,
            tokio_runtime,
            ..
        } = self;

        BaseAddOns::new(
            RpcAddOns::new(
                BaseEthApiBuilder::default()
                    .with_sequencer(sequencer_url.clone())
                    .with_sequencer_headers(sequencer_headers.clone())
                    .with_min_suggested_priority_fee(min_suggested_priority_fee),
                PVB::default(),
                EB::default(),
                EVB::default(),
                rpc_middleware,
            )
            .with_tokio_runtime(tokio_runtime),
            da_config.unwrap_or_default(),
            gas_limit_config.unwrap_or_default(),
            sequencer_url,
            sequencer_headers,
            min_suggested_priority_fee,
        )
    }
}

/// A regular Base EVM and executor builder.
#[derive(Debug, Copy, Clone, Default)]
#[non_exhaustive]
pub struct BaseExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for BaseExecutorBuilder
where
    Node: FullNodeTypes<Types: BaseNodeTypes>,
{
    type EVM = BaseEvmConfig<
        <Node::Types as NodeTypes>::ChainSpec,
        <Node::Types as NodeTypes>::Primitives,
    >;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = BaseEvmConfig::new(ctx.chain_spec(), BaseRethReceiptBuilder::default());

        Ok(evm_config)
    }
}

/// A basic Base transaction pool.
///
/// This contains various settings that can be configured and take precedence over the node's
/// config.
#[derive(Debug)]
pub struct BasePoolBuilder<T = BasePooledTransaction> {
    /// Enforced overrides that are applied to the pool config.
    pub pool_config_overrides: PoolBuilderConfigOverrides,
    /// The ordering strategy for the transaction pool.
    pub ordering: BaseOrdering<T>,
    /// Marker for the pooled transaction type.
    _pd: core::marker::PhantomData<T>,
}

impl<T> Default for BasePoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            ordering: BaseOrdering::default(),
            _pd: Default::default(),
        }
    }
}

impl<T> Clone for BasePoolBuilder<T> {
    fn clone(&self) -> Self {
        Self {
            pool_config_overrides: self.pool_config_overrides.clone(),
            ordering: self.ordering.clone(),
            _pd: core::marker::PhantomData,
        }
    }
}

impl<T> BasePoolBuilder<T> {
    /// Sets the [`PoolBuilderConfigOverrides`] on the pool builder.
    pub fn with_pool_config_overrides(
        mut self,
        pool_config_overrides: PoolBuilderConfigOverrides,
    ) -> Self {
        self.pool_config_overrides = pool_config_overrides;
        self
    }

    /// Sets the ordering strategy for the transaction pool.
    pub const fn with_ordering(mut self, ordering: BaseOrdering<T>) -> Self {
        self.ordering = ordering;
        self
    }
}

impl<Node, T, Evm> PoolBuilder<Node, Evm> for BasePoolBuilder<T>
where
    Node: FullNodeTypes<Types: BaseNodeTypes>,
    T: EthPoolTransaction<Consensus = TxTy<Node::Types>> + BasePooledTx + TimestampedTransaction,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Node::Types>> + Clone + 'static,
{
    type Pool = BaseTransactionPool<Node::Provider, DiskFileBlobStore, Evm, T, BaseOrdering<T>>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let Self { pool_config_overrides, ordering, .. } = self;

        let blob_store = reth_node_builder::components::create_blob_store(ctx)?;
        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .no_eip4844()
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .kzg_settings(ctx.kzg_settings()?)
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(
                    pool_config_overrides
                        .additional_validation_tasks
                        .unwrap_or_else(|| ctx.config().txpool.additional_validation_tasks),
                )
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
                .map(|validator| {
                    BaseTransactionValidator::new(validator)
                        // In --dev mode we can't require gas fees because we're unable to decode
                        // the L1 block info
                        .require_l1_data_gas_fee(!ctx.config().dev.dev)
                });

        let final_pool_config = pool_config_overrides.apply(ctx.pool_config());

        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_with_ordering_and_spawn_maintenance_task(
                ordering,
                blob_store,
                final_pool_config,
            )?;

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

/// A basic Base payload service builder
#[derive(Debug, Default, Clone)]
pub struct BasePayloadBuilder<Txs = ()> {
    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    pub compute_pending_block: bool,
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
    /// This data availability configuration specifies constraints for the payload builder
    /// when assembling payloads
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the OP builder.
    /// This is used to configure gas limit related constraints for the payload builder.
    pub gas_limit_config: GasLimitConfig,
}

impl BasePayloadBuilder {
    /// Create a new instance with the given `compute_pending_block` flag and data availability
    /// config.
    pub fn new(compute_pending_block: bool) -> Self {
        Self {
            compute_pending_block,
            best_transactions: (),
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
        }
    }

    /// Configure the data availability configuration for the OP payload builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = da_config;
        self
    }

    /// Configure the gas limit configuration for the OP payload builder.
    pub fn with_gas_limit_config(mut self, gas_limit_config: GasLimitConfig) -> Self {
        self.gas_limit_config = gas_limit_config;
        self
    }
}

impl<Txs> BasePayloadBuilder<Txs> {
    /// Configures the type responsible for yielding the transactions that should be included in the
    /// payload.
    pub fn with_transactions<T>(self, best_transactions: T) -> BasePayloadBuilder<T> {
        let Self { compute_pending_block, da_config, gas_limit_config, .. } = self;
        BasePayloadBuilder { compute_pending_block, best_transactions, da_config, gas_limit_config }
    }
}

impl<Node, Pool, Txs, Evm, Attrs> PayloadBuilderBuilder<Node, Pool, Evm> for BasePayloadBuilder<Txs>
where
    Node: FullNodeTypes<
            Provider: ChainSpecProvider<ChainSpec: Upgrades>,
            Types: NodeTypes<
                Primitives: PayloadPrimitives,
                Payload: PayloadTypes<
                    BuiltPayload = BaseBuiltPayload<PrimitivesTy<Node::Types>>,
                    PayloadBuilderAttributes = Attrs,
                >,
            >,
        >,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Node::Types>,
            NextBlockEnvCtx: BuildNextEnv<
                Attrs,
                HeaderTy<Node::Types>,
                <Node::Types as NodeTypes>::ChainSpec,
            >,
        > + 'static,
    Pool:
        TransactionPool<Transaction: BasePooledTx<Consensus = TxTy<Node::Types>>> + Unpin + 'static,
    Txs: BasePayloadTransactions<Pool::Transaction>,
    Attrs: Attributes<Transaction = TxTy<Node::Types>>,
{
    type PayloadBuilder =
        base_execution_payload_builder::BasePayloadBuilder<Pool, Node::Provider, Evm, Txs, Attrs>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let payload_builder =
            base_execution_payload_builder::BasePayloadBuilder::with_builder_config(
                pool,
                ctx.provider().clone(),
                evm_config,
                BaseBuilderConfig {
                    da_config: self.da_config.clone(),
                    gas_limit_config: self.gas_limit_config.clone(),
                },
            )
            .with_transactions(self.best_transactions.clone())
            .set_compute_pending_block(self.compute_pending_block);
        Ok(payload_builder)
    }
}

/// A basic Base network builder.
#[derive(Debug, Clone)]
pub struct BaseNetworkBuilder {
    /// Disable transaction pool gossip
    pub disable_txpool_gossip: bool,
    /// Disable discovery v4
    pub disable_discovery_v4: bool,
    /// Enable the Base discv5 protocol identity
    pub base_protocol: bool,
}

impl Default for BaseNetworkBuilder {
    fn default() -> Self {
        Self { disable_discovery_v4: false, disable_txpool_gossip: false, base_protocol: true }
    }
}

impl BaseNetworkBuilder {
    /// Creates a new `BaseNetworkBuilder`.
    pub const fn new(
        disable_txpool_gossip: bool,
        disable_discovery_v4: bool,
        base_protocol: bool,
    ) -> Self {
        Self { disable_txpool_gossip, disable_discovery_v4, base_protocol }
    }
}

fn block_on<T>(f: impl Future<Output = T>) -> T {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| runtime.block_on(f))
    } else {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }
}

impl BaseNetworkBuilder {
    /// Returns the [`NetworkConfig`] that contains the settings to launch the p2p network.
    ///
    /// This applies the configured [`BaseNetworkBuilder`] settings.
    pub fn network_config<Node, NetworkP>(
        &self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<NetworkConfig<Node::Provider, NetworkP>>
    where
        Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
        NetworkP: NetworkPrimitives,
    {
        let disable_txpool_gossip = self.disable_txpool_gossip;
        let disable_discovery_v4 = self.disable_discovery_v4;
        let base_protocol = self.base_protocol;
        let args = &ctx.config().network;
        let network_builder = ctx
            .network_config_builder()?
            // apply discovery settings
            .apply(|mut builder| {
                let rlpx_socket = (args.addr, args.port).into();
                if disable_discovery_v4 || args.discovery.disable_discovery {
                    builder = builder.disable_discv4_discovery();
                }
                if !args.discovery.disable_discovery {
                    // copied from discovery_v5_builder to override discv5_config
                    let discv5_addr_ipv4 =
                        args.discovery.discv5_addr.or_else(|| match rlpx_socket {
                            std::net::SocketAddr::V4(addr) => Some(*addr.ip()),
                            std::net::SocketAddr::V6(_) => None,
                        });
                    let discv5_addr_ipv6 =
                        args.discovery.discv5_addr_ipv6.or_else(|| match rlpx_socket {
                            std::net::SocketAddr::V4(_) => None,
                            std::net::SocketAddr::V6(addr) => Some(*addr.ip()),
                        });
                    let listen_config = reth_discv5::discv5::ListenConfig::from_two_sockets(
                        discv5_addr_ipv4
                            .map(|addr| SocketAddrV4::new(addr, args.discovery.discv5_port)),
                        discv5_addr_ipv6.map(|addr| {
                            SocketAddrV6::new(addr, args.discovery.discv5_port_ipv6, 0, 0)
                        }),
                    );

                    let external_addr = block_on(args.nat.clone().external_addr());

                    let mut discv5_config_builder =
                        reth_discv5::discv5::ConfigBuilder::new(listen_config);
                    if base_protocol {
                        discv5_config_builder.protocol_identity(
                            reth_discv5::discv5::ProtocolIdentity {
                                protocol_id: BASE_V0_PROTOCOL_VERSION,
                                ..Default::default()
                            },
                        );
                    }

                    let mut reth_config_builder = args
                        .discovery
                        .discovery_v5_builder(
                            rlpx_socket,
                            ctx.config()
                                .network
                                .resolved_bootnodes()
                                .or_else(|| ctx.chain_spec().bootnodes())
                                .unwrap_or_default(),
                        )
                        .discv5_config(discv5_config_builder.build());

                    reth_config_builder = match external_addr {
                        Some(std::net::IpAddr::V4(addr)) => {
                            let addr = addr.octets();
                            let mut out = BytesMut::with_capacity(addr.length());
                            addr.encode(&mut out);
                            reth_config_builder
                                .add_enr_kv_pair(IP_ENR_KEY, Bytes::from(out.freeze()))
                        }
                        Some(std::net::IpAddr::V6(addr)) => {
                            let addr = addr.octets();
                            let mut out = BytesMut::with_capacity(addr.length());
                            addr.encode(&mut out);
                            reth_config_builder
                                .add_enr_kv_pair(IP6_ENR_KEY, Bytes::from(out.freeze()))
                        }
                        _ => reth_config_builder,
                    };

                    builder = builder.discovery_v5(reth_config_builder);
                }

                builder
            });

        let mut network_config = ctx.build_network_config(network_builder);

        // When `sequencer_endpoint` is configured, the node will forward all transactions to a
        // Sequencer node for execution and inclusion on L1, and disable its own txpool
        // gossip to prevent other parties in the network from learning about them.
        network_config.tx_gossip_disabled = disable_txpool_gossip;

        Ok(network_config)
    }
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for BaseNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
{
    type Network =
        NetworkHandle<BasicNetworkPrimitives<PrimitivesTy<Node::Types>, PoolPooledTx<Pool>>>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network_config = self.network_config(ctx)?;
        let network = NetworkManager::builder(network_config).await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");

        Ok(handle)
    }
}

/// A basic Base consensus builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BaseConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BaseConsensusBuilder
where
    Node: FullNodeTypes<Types: BaseNodeTypes>,
{
    type Consensus = Arc<BaseBeaconConsensus>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(BaseBeaconConsensus::new(ctx.chain_spec())))
    }
}

/// Builder for [`BaseEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BasePayloadValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for BasePayloadValidatorBuilder
where
    Node: FullNodeComponents<
        Types: NodeTypes<ChainSpec: Upgrades, Payload: PayloadTypes<ExecutionData = ExecutionData>>,
    >,
{
    type Validator = BaseEngineValidator<
        Node::Provider,
        <<Node::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx,
        <Node::Types as NodeTypes>::ChainSpec,
    >;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(BaseEngineValidator::new::<KeccakKeyHasher>(
            Arc::clone(&ctx.config.chain),
            ctx.node.provider().clone(),
        ))
    }
}

/// Network primitive types used by Base networks.
pub type BaseNetworkPrimitives = BasicNetworkPrimitives<BasePrimitives, BasePooledTransaction>;
