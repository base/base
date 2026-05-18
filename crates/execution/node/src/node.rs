//! Base Node types config.

use std::{
    marker::PhantomData,
    net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6},
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
use base_execution_txpool::{
    BaseOrdering, BasePooledTransaction, BasePooledTx, BaseTransactionPool,
    BaseTransactionValidator, TimestampedTransaction,
};
use reth_chainspec::{BaseFeeParams, ChainSpecProvider, EthChainSpec, Hardforks};
use reth_discv5::discv5::enr::{IP_ENR_KEY, IP6_ENR_KEY};
use reth_evm::ConfigureEvm;
use reth_network::{
    NetworkConfig, NetworkConfigBuilder, NetworkHandle, NetworkManager, NetworkPrimitives,
    PeersInfo, types::BasicNetworkPrimitives,
};
use reth_network_peers::NodeRecord;
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
use reth_node_core::args::{DiscoveryArgs, NetworkArgs as RethNetworkArgs};
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
    BaseEngineApiBuilder, BaseEngineTypes, BaseStorage,
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
    /// Data availability configuration for the payload builder.
    ///
    /// Used to throttle the size of the data availability payloads (configured by the batcher via
    /// the `miner_` api).
    ///
    /// By default no throttling is applied.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the payload builder.
    /// Used to control the gas limit of the blocks produced by the payload builder (configured by the
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

    /// Configure the data availability configuration for the payload builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = da_config;
        self
    }

    /// Configure the gas limit configuration for the payload builder.
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
            max_inflight_delegated_slots,
            ..
        } = self.args;
        let ordering = match txpool_ordering {
            TxpoolOrdering::CoinbaseTip => BaseOrdering::coinbase_tip(),
            TxpoolOrdering::Timestamp => BaseOrdering::timestamp(),
        };
        ComponentsBuilder::default()
            .node_types::<Node>()
            .executor(BaseExecutorBuilder::default())
            .pool(
                BasePoolBuilder::default()
                    .with_ordering(ordering)
                    .with_max_inflight_delegated_slots(max_inflight_delegated_slots),
            )
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
    /// # Open a `ProviderFactory` in read-only mode from a datadir
    ///
    /// See also: [`ProviderFactoryBuilder`] and
    /// [`ReadOnlyConfig`](reth_provider::providers::ReadOnlyConfig).
    ///
    /// ```no_run
    /// use base_execution_chainspec::BaseChainSpec;
    /// use base_node_core::BaseNode;
    /// use std::sync::Arc;
    ///
    /// fn demo(runtime: reth_tasks::Runtime) {
    ///     let factory = BaseNode::provider_factory_builder()
    ///         .open_read_only(Arc::new(BaseChainSpec::mainnet()), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// # Open a `ProviderFactory` with custom config
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
    /// Data availability configuration for the payload builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the payload builder.
    pub gas_limit_config: GasLimitConfig,
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
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
        // Install additional rollup-specific RPC methods.
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
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Data availability configuration for the payload builder.
    da_config: Option<BaseDAConfig>,
    /// Gas limit configuration for the payload builder.
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
    /// Maximum inflight EIP-7702 delegated account transactions per sender.
    pub max_inflight_delegated_slots: usize,
    /// Marker for the pooled transaction type.
    _pd: core::marker::PhantomData<T>,
}

impl<T> Default for BasePoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            ordering: BaseOrdering::default(),
            max_inflight_delegated_slots: 1,
            _pd: Default::default(),
        }
    }
}

impl<T> Clone for BasePoolBuilder<T> {
    fn clone(&self) -> Self {
        Self {
            pool_config_overrides: self.pool_config_overrides.clone(),
            ordering: self.ordering.clone(),
            max_inflight_delegated_slots: self.max_inflight_delegated_slots,
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

    /// Sets the maximum inflight EIP-7702 delegated account transactions per sender.
    pub const fn with_max_inflight_delegated_slots(mut self, limit: usize) -> Self {
        self.max_inflight_delegated_slots = limit;
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
        let Self { pool_config_overrides, ordering, max_inflight_delegated_slots, .. } = self;

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

        let mut final_pool_config = pool_config_overrides.apply(ctx.pool_config());
        final_pool_config.max_inflight_delegated_slot_limit = max_inflight_delegated_slots;

        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_with_ordering_and_spawn_maintenance_task(
                ordering,
                blob_store,
                final_pool_config,
            )?;

        info!(target: "reth::cli", max_inflight_delegated_slots, "Transaction pool initialized");
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
    /// Gas limit configuration for the payload builder.
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

    /// Configure the data availability configuration for the payload builder.
    pub fn with_da_config(mut self, da_config: BaseDAConfig) -> Self {
        self.da_config = da_config;
        self
    }

    /// Configure the gas limit configuration for the payload builder.
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

    /// Runs a future on the current runtime, or creates one when needed.
    pub fn block_on<T>(f: impl Future<Output = T>) -> T {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| runtime.block_on(f))
        } else {
            tokio::runtime::Runtime::new().unwrap().block_on(f)
        }
    }
}

/// Base-specific discovery configuration.
#[derive(Debug, Clone)]
pub struct BaseDiscoveryConfig {
    /// Disable discovery v4.
    pub disable_discovery_v4: bool,
    /// Enable the Base discv5 protocol identity.
    pub base_protocol: bool,
}

impl BaseDiscoveryConfig {
    /// Creates a new discovery config.
    pub const fn new(disable_discovery_v4: bool, base_protocol: bool) -> Self {
        Self { disable_discovery_v4, base_protocol }
    }

    /// Returns true if discv4 discovery should be disabled.
    pub const fn should_disable_discv4(&self, discovery: &DiscoveryArgs) -> bool {
        self.disable_discovery_v4
            || discovery.disable_discovery
            || discovery.disable_discv4_discovery
    }

    /// Applies Base discovery settings to the reth network config builder.
    pub fn apply_to_network_builder<N>(
        &self,
        mut builder: NetworkConfigBuilder<N>,
        args: &RethNetworkArgs,
        boot_nodes: impl IntoIterator<Item = NodeRecord>,
        external_addr: Option<IpAddr>,
    ) -> NetworkConfigBuilder<N>
    where
        N: NetworkPrimitives,
    {
        if self.should_disable_discv4(&args.discovery) {
            builder = builder.disable_discv4_discovery();
        }

        if !args.discovery.disable_discovery {
            builder =
                builder.discovery_v5(self.discovery_v5_builder(args, boot_nodes, external_addr));
        }

        builder
    }

    /// Creates the Base discv5 config builder from reth network arguments.
    pub fn discovery_v5_builder(
        &self,
        args: &RethNetworkArgs,
        boot_nodes: impl IntoIterator<Item = NodeRecord>,
        external_addr: Option<IpAddr>,
    ) -> reth_discv5::ConfigBuilder {
        let rlpx_socket = Self::rlpx_socket(args);
        let mut builder = args
            .discovery
            .discovery_v5_builder(rlpx_socket, boot_nodes)
            .discv5_config(self.discv5_config(args));

        if let Some((key, value)) = Self::enr_ip_kv_pair(external_addr) {
            builder = builder.add_enr_kv_pair(key, value);
        }

        builder
    }

    /// Creates the inner discv5 config with Base protocol identity when enabled.
    pub fn discv5_config(&self, args: &RethNetworkArgs) -> reth_discv5::discv5::Config {
        let mut builder = reth_discv5::discv5::ConfigBuilder::new(Self::discv5_listen_config(args));

        if self.base_protocol {
            builder.protocol_identity(reth_discv5::discv5::ProtocolIdentity {
                protocol_id: BASE_V0_PROTOCOL_VERSION,
                ..Default::default()
            });
        }

        builder.build()
    }

    /// Creates the discv5 listen config from reth network arguments.
    ///
    /// Note: reth's `build()` always overwrites the discv5 IPv4/IPv6 address with the `RLPx`
    /// address, because ENR has no mechanism to advertise different addresses for `RLPx` and
    /// discv5. As a result, `discv5_addr` only influences the UDP listen port, not the
    /// advertised IP.
    pub fn discv5_listen_config(args: &RethNetworkArgs) -> reth_discv5::discv5::ListenConfig {
        let rlpx_socket = Self::rlpx_socket(args);
        let discv5_addr_ipv4 = args.discovery.discv5_addr.or_else(|| match rlpx_socket {
            SocketAddr::V4(addr) => Some(*addr.ip()),
            SocketAddr::V6(_) => None,
        });
        let discv5_addr_ipv6 = args.discovery.discv5_addr_ipv6.or_else(|| match rlpx_socket {
            SocketAddr::V4(_) => None,
            SocketAddr::V6(addr) => Some(*addr.ip()),
        });

        reth_discv5::discv5::ListenConfig::from_two_sockets(
            discv5_addr_ipv4.map(|addr| SocketAddrV4::new(addr, args.discovery.discv5_port)),
            discv5_addr_ipv6
                .map(|addr| SocketAddrV6::new(addr, args.discovery.discv5_port_ipv6, 0, 0)),
        )
    }

    /// Returns the `RLPx` socket configured by reth network arguments.
    pub fn rlpx_socket(args: &RethNetworkArgs) -> SocketAddr {
        (args.addr, args.port).into()
    }

    /// Encodes the NAT-discovered external IP as an ENR key-value pair.
    pub fn enr_ip_kv_pair(external_addr: Option<IpAddr>) -> Option<(&'static [u8], Bytes)> {
        match external_addr {
            Some(IpAddr::V4(addr)) => {
                let addr = addr.octets();
                let mut out = BytesMut::with_capacity(addr.length());
                addr.encode(&mut out);
                Some((IP_ENR_KEY, Bytes::from(out.freeze())))
            }
            Some(IpAddr::V6(addr)) => {
                let addr = addr.octets();
                let mut out = BytesMut::with_capacity(addr.length());
                addr.encode(&mut out);
                Some((IP6_ENR_KEY, Bytes::from(out.freeze())))
            }
            None => None,
        }
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
        let discovery_config =
            BaseDiscoveryConfig::new(self.disable_discovery_v4, self.base_protocol);
        let args = &ctx.config().network;
        let network_builder = ctx
            .network_config_builder()?
            // apply discovery settings
            .apply(|builder| {
                let external_addr = if args.discovery.disable_discovery {
                    None
                } else {
                    Self::block_on(args.nat.clone().external_addr())
                };
                discovery_config.apply_to_network_builder(
                    builder,
                    args,
                    ctx.config()
                        .network
                        .resolved_bootnodes()
                        .or_else(|| ctx.chain_spec().bootnodes())
                        .unwrap_or_default(),
                    external_addr,
                )
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

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        sync::Arc,
    };

    use reth_chainspec::MAINNET;
    use reth_discv5::{
        build_local_enr,
        discv5::{ListenConfig, ProtocolIdentity},
    };
    use reth_network::{EthNetworkPrimitives, NetworkConfigBuilder, config::rng_secret_key};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::enabled(false, false, false, false)]
    #[case::disabled_by_base(true, false, false, true)]
    #[case::disabled_by_reth(false, true, false, true)]
    #[case::disabled_by_global_discovery(false, false, true, true)]
    fn discv4_disable_decision_uses_base_and_reth_flags(
        #[case] disable_by_base: bool,
        #[case] disable_by_reth: bool,
        #[case] disable_all_discovery: bool,
        #[case] expected: bool,
    ) {
        let discovery_args = DiscoveryArgs {
            disable_discovery: disable_all_discovery,
            disable_discv4_discovery: disable_by_reth,
            ..Default::default()
        };
        let discovery_config = BaseDiscoveryConfig::new(disable_by_base, true);

        assert_eq!(discovery_config.should_disable_discv4(&discovery_args), expected);
    }

    #[rstest]
    #[case::enabled(false, false, false, true)]
    #[case::disabled_by_base(true, false, false, false)]
    #[case::disabled_by_reth(false, true, false, false)]
    #[case::disabled_by_global_discovery(false, false, true, false)]
    fn discovery_config_applies_discv4_setting(
        #[case] disable_by_base: bool,
        #[case] disable_by_reth: bool,
        #[case] disable_all_discovery: bool,
        #[case] expected_enabled: bool,
    ) {
        let mut args = RethNetworkArgs::default();
        args.discovery.disable_discovery = disable_all_discovery;
        args.discovery.disable_discv4_discovery = disable_by_reth;
        let discovery_config = BaseDiscoveryConfig::new(disable_by_base, true);

        let network_config = discovery_config
            .apply_to_network_builder(
                NetworkConfigBuilder::<EthNetworkPrimitives>::with_rng_secret_key(),
                &args,
                Vec::<NodeRecord>::new(),
                None,
            )
            .build_with_noop_provider(Arc::clone(&MAINNET));

        assert_eq!(network_config.discovery_v4_config.is_some(), expected_enabled);
    }

    #[rstest]
    #[case::enabled(false, true)]
    #[case::disabled(true, false)]
    fn discovery_config_applies_discv5_setting(
        #[case] disable_all_discovery: bool,
        #[case] expected_enabled: bool,
    ) {
        let mut args = RethNetworkArgs::default();
        args.discovery.disable_discovery = disable_all_discovery;
        let discovery_config = BaseDiscoveryConfig::new(false, true);

        let network_config = discovery_config
            .apply_to_network_builder(
                NetworkConfigBuilder::<EthNetworkPrimitives>::with_rng_secret_key(),
                &args,
                Vec::<NodeRecord>::new(),
                None,
            )
            .build_with_noop_provider(Arc::clone(&MAINNET));

        assert_eq!(network_config.discovery_v5_config.is_some(), expected_enabled);
    }

    #[rstest]
    #[case::base_protocol_enabled(true, BASE_V0_PROTOCOL_VERSION)]
    #[case::default_protocol(false, ProtocolIdentity::default().protocol_id)]
    fn discv5_config_uses_protocol_identity(
        #[case] base_protocol: bool,
        #[case] expected_protocol_id: [u8; 6],
    ) {
        let args = RethNetworkArgs::default();
        let discovery_config = BaseDiscoveryConfig::new(false, base_protocol);

        let discv5_config = discovery_config.discv5_config(&args);

        assert_eq!(discv5_config.protocol_identity.protocol_id, expected_protocol_id);
    }

    #[rstest]
    #[case::rlpx_ipv4(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        None,
        None,
        9201,
        9202,
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        9201
    )]
    #[case::explicit_ipv4_overwritten_by_rlpx(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        Some(Ipv4Addr::new(203, 0, 113, 1)),
        None,
        9201,
        9202,
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        9201
    )]
    #[case::rlpx_ipv6(
        IpAddr::V6("2001:db8::1".parse().expect("valid ipv6")),
        None,
        None,
        9201,
        9202,
        IpAddr::V6("2001:db8::1".parse().expect("valid ipv6")),
        9202
    )]
    #[case::dual_stack(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        None,
        Some("2001:db8::2".parse().expect("valid ipv6")),
        9201,
        9202,
        IpAddr::V6("2001:db8::2".parse().expect("valid ipv6")),
        9202
    )]
    fn discv5_listen_config_uses_explicit_addresses_or_rlpx_fallback(
        #[case] rlpx_ip: IpAddr,
        #[case] discv5_addr: Option<Ipv4Addr>,
        #[case] discv5_addr_ipv6: Option<Ipv6Addr>,
        #[case] discv5_port: u16,
        #[case] discv5_port_ipv6: u16,
        #[case] expected_advertised_ip: IpAddr,
        #[case] expected_advertised_port: u16,
    ) {
        let mut args = RethNetworkArgs { addr: rlpx_ip, port: 30303, ..Default::default() };
        args.discovery.discv5_addr = discv5_addr;
        args.discovery.discv5_addr_ipv6 = discv5_addr_ipv6;
        args.discovery.discv5_port = discv5_port;
        args.discovery.discv5_port_ipv6 = discv5_port_ipv6;
        let discovery_config = BaseDiscoveryConfig::new(false, true);

        let reth_discv5_config =
            discovery_config.discovery_v5_builder(&args, Vec::<NodeRecord>::new(), None).build();

        assert_eq!(
            reth_discv5_config.discovery_socket(),
            SocketAddr::new(expected_advertised_ip, expected_advertised_port)
        );
        assert_eq!(reth_discv5_config.rlpx_socket(), &SocketAddr::new(rlpx_ip, args.port));
    }

    #[rstest]
    #[case::ipv4(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)))]
    #[case::ipv6(IpAddr::V6("2001:db8::10".parse().expect("valid ipv6")))]
    fn discovery_v5_builder_advertises_external_ip(#[case] external_addr: IpAddr) {
        let args =
            RethNetworkArgs { addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED), ..Default::default() };
        let discovery_config = BaseDiscoveryConfig::new(false, true);

        let reth_discv5_config = discovery_config
            .discovery_v5_builder(&args, Vec::<NodeRecord>::new(), Some(external_addr))
            .build();
        let secret_key = rng_secret_key();
        let (enr, _, _, _) = build_local_enr(&secret_key, &reth_discv5_config);

        match external_addr {
            IpAddr::V4(addr) => assert_eq!(enr.ip4(), Some(addr)),
            IpAddr::V6(addr) => assert_eq!(enr.ip6(), Some(addr)),
        }
    }

    #[rstest]
    #[case::ipv4(
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        ListenConfig::Ipv4 { ip: Ipv4Addr::new(192, 0, 2, 1), port: 9200 }
    )]
    #[case::ipv6(
        IpAddr::V6("2001:db8::1".parse().expect("valid ipv6")),
        ListenConfig::Ipv6 { ip: "2001:db8::1".parse().expect("valid ipv6"), port: 9200 }
    )]
    fn discv5_inner_listen_config_matches_rlpx_ip(
        #[case] rlpx_ip: IpAddr,
        #[case] expected: ListenConfig,
    ) {
        let args = RethNetworkArgs { addr: rlpx_ip, port: 30303, ..Default::default() };
        let discovery_config = BaseDiscoveryConfig::new(false, true);

        let discv5_config = discovery_config.discv5_config(&args);

        match (discv5_config.listen_config, expected) {
            (
                ListenConfig::Ipv4 { ip, port },
                ListenConfig::Ipv4 { ip: expected_ip, port: expected_port },
            ) => {
                assert_eq!(ip, expected_ip);
                assert_eq!(port, expected_port);
            }
            (
                ListenConfig::Ipv6 { ip, port },
                ListenConfig::Ipv6 { ip: expected_ip, port: expected_port },
            ) => {
                assert_eq!(ip, expected_ip);
                assert_eq!(port, expected_port);
            }
            (actual, expected) => {
                panic!("unexpected listen config: actual={actual:?} expected={expected:?}")
            }
        }
    }
}
