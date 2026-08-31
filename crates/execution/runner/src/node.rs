//! Base Node types config.

use base_common_consensus::BasePrimitives;
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};
use base_execution_rpc::eth::BaseEthApiBuilder;
use base_execution_txpool::GuardLimits;
use base_node_core::{
    BaseConsensusBuilder, BaseEngineApiBuilder, BaseEngineTypes, BaseExecutorBuilder,
    BaseNetworkBuilder, BaseNodeComponentBuilder, BaseNodeTypes, BasePayloadValidatorBuilder,
    BaseStorage,
    args::RollupArgs,
    node::{BasePayloadBuilder, BasePayloadServiceBuilder, BasePoolBuilder},
};
use reth_node_builder::{
    Node, NodeAdapter, NodeComponentsBuilder,
    components::ComponentsBuilder,
    node::{FullNodeTypes, NodeTypes},
    rpc::BasicEngineValidatorBuilder,
};
use reth_provider::providers::ProviderFactoryBuilder;
use reth_rpc_api::eth::RpcTypes;

use crate::{BaseAddOns, BaseAddOnsBuilder};

/// Type configuration for a regular Base node.
#[derive(Debug, Clone)]
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
    /// Whether to drop positively stale EIP-8130 transactions using their
    /// captured authorization manifest before execution.
    pub manifest_precheck_enabled: bool,
}

impl Default for BaseNode {
    fn default() -> Self {
        Self::new(RollupArgs::default())
    }
}

impl BaseNode {
    /// Creates a new instance of the Base node type.
    pub fn new(args: RollupArgs) -> Self {
        Self {
            args,
            da_config: BaseDAConfig::default(),
            gas_limit_config: GasLimitConfig::default(),
            manifest_precheck_enabled: true,
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

    /// Configure whether EIP-8130 authorization manifests are checked before execution.
    pub const fn with_manifest_precheck_enabled(mut self, enabled: bool) -> Self {
        self.manifest_precheck_enabled = enabled;
        self
    }

    /// Returns the components for the given [`RollupArgs`].
    pub fn components<Node>(&self) -> BaseNodeComponentBuilder<Node>
    where
        Node: FullNodeTypes<Types: BaseNodeTypes>,
    {
        let RollupArgs {
            discovery_v4,
            max_inflight_delegated_slots,
            mempool_sender_limit,
            mempool_payer_limit,
            ..
        } = self.args;
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(
                BasePoolBuilder::default()
                    .with_max_inflight_delegated_slots(max_inflight_delegated_slots)
                    .with_guard_limits(GuardLimits {
                        signature_limit: mempool_sender_limit,
                        payment_limit: mempool_payer_limit,
                        ..Default::default()
                    })
                    .with_additional_trusted_delegation_targets(
                        self.args.mempool_trusted_delegation_targets.iter().copied(),
                    ),
            )
            .executor(BaseExecutorBuilder::default())
            .payload(BasePayloadServiceBuilder::new(
                BasePayloadBuilder::new()
                    .with_da_config(self.da_config.clone())
                    .with_gas_limit_config(self.gas_limit_config.clone())
                    .with_manifest_precheck_enabled(self.manifest_precheck_enabled),
            ))
            .network(BaseNetworkBuilder::new(!discovery_v4))
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
    /// use base_node_runner::BaseNode;
    /// use reth_provider::providers::ReadOnlyConfig;
    /// use std::sync::Arc;
    ///
    /// let runtime = reth_tasks::Runtime::test();
    /// let factory = BaseNode::provider_factory_builder()
    ///     .open_read_only(
    ///         Arc::new(BaseChainSpec::mainnet()),
    ///         ReadOnlyConfig::from_datadir("datadir").no_watch(),
    ///         runtime,
    ///     )
    ///     .unwrap();
    /// ```
    ///
    /// # Open a `ProviderFactory` manually with all required components
    ///
    /// ```no_run
    /// use base_execution_chainspec::BaseChainSpecBuilder;
    /// use base_node_runner::BaseNode;
    /// use reth_db::mdbx::DatabaseArguments;
    /// use reth_provider::providers::ReadOnlyConfig;
    ///
    /// let runtime = reth_tasks::Runtime::test();
    /// let factory = BaseNode::provider_factory_builder()
    ///     .open_read_only(
    ///         BaseChainSpecBuilder::base_mainnet().build().into(),
    ///         ReadOnlyConfig {
    ///             db_dir: "db".into(),
    ///             db_args: DatabaseArguments::default(),
    ///             static_files_dir: "db/static_files".into(),
    ///             rocksdb_dir: "db/rocksdb".into(),
    ///             watch_static_files: false,
    ///         },
    ///         runtime,
    ///     )
    ///     .unwrap();
    /// ```
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl<N> Node<N> for BaseNode
where
    N: FullNodeTypes<Types: BaseNodeTypes>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BasePoolBuilder,
        BasePayloadServiceBuilder,
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

impl NodeTypes for BaseNode {
    type Primitives = BasePrimitives;
    type ChainSpec = BaseChainSpec;
    type Storage = BaseStorage;
    type Payload = BaseEngineTypes;
}
