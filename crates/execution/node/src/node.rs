//! Base Node types config.

use std::{
    net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::Arc,
};

use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Address, B64, B256, Bytes, bytes::BytesMut};
use alloy_rlp::Encodable;
use base_common_chain_config::BaseChainSpec;
use base_common_chain_config::Upgrades;
use base_common_types_chain::BlockHeader;
use base_common_types_payload::BasePayloadAttributes;
use base_execution_evm_blocks::BaseEvmConfig;
use base_execution_payload_builder::{
    BasePayloadBuilderAttributes, RejectionCache,
    config::{BaseDAConfig, GasLimitConfig, ResourceMeteringConfig},
};
use base_execution_payload_types::PayloadAttributesBuilder;
use base_execution_txpool::{
    BaseOrdering, BaseTransactionPool, BaseTransactionValidator, DiskFileBlobStore, GuardLimits,
    TransactionValidationTaskExecutor, maintain_state_diff_invalidation,
};
use reth_chain_state::CanonStateSubscriptions;
use reth_discv5::discv5::enr::{IP_ENR_KEY, IP6_ENR_KEY};
use reth_network::{NetworkConfig, NetworkConfigBuilder, NetworkHandle, NetworkManager, PeersInfo};
use reth_network_peers::NodeRecord;
use reth_node_core::args::{DiscoveryArgs, NetworkArgs as RethNetworkArgs};
use reth_primitives_traits::SealedHeader;
use reth_provider::providers::{BlockchainProvider, ProviderFactoryBuilder};
use reth_tracing::tracing::{debug, info};
use tokio_stream::wrappers::BroadcastStream;

use crate::{
    BasePayloadServiceConfig, BuilderContext,
    args::{RollupArgs, TxpoolOrdering},
    spawn_maintenance_tasks,
};

/// Discovery v5 protocol version for Base.
pub const BASE_V0_PROTOCOL_VERSION: [u8; 6] = *b"basev0";

/// Local payload attributes builder for Base.
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

impl PayloadAttributesBuilder<BasePayloadBuilderAttributes> for BaseLocalPayloadAttributesBuilder {
    fn build(&self, parent: &SealedHeader) -> BasePayloadBuilderAttributes {
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

        let attributes = BasePayloadAttributes {
            payload_attributes: base_common_types_payload::PayloadAttributes {
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
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: Some(vec![TX_SET_L1_BLOCK_BASE_MAINNET_BLOCK_1.into()]),
            no_tx_pool: None,
            gas_limit,
            eip_1559_params,
            min_base_fee: Some(0),
        };

        BasePayloadBuilderAttributes::try_new(parent.hash(), attributes, 3)
            .expect("static dev payload attributes must decode")
    }
}

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
    /// Resource metering by opcode for native payload admission.
    pub resource_metering: ResourceMeteringConfig,
    /// Shared, cross-job cache of permanently rejected transaction hashes.
    pub rejection_cache: RejectionCache,
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
            resource_metering: ResourceMeteringConfig::default(),
            rejection_cache: RejectionCache::default(),
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

    /// Configure resource metering by opcode for the native payload builder.
    pub fn with_resource_metering(mut self, resource_metering: ResourceMeteringConfig) -> Self {
        self.resource_metering = resource_metering;
        self
    }

    /// Configure the shared rejection cache for permanently rejected transactions.
    pub fn with_rejection_cache(mut self, rejection_cache: RejectionCache) -> Self {
        self.rejection_cache = rejection_cache;
        self
    }

    /// Builds the fixed Base execution components.
    pub async fn build_components(
        &self,
        ctx: &BuilderContext,
        payload: Option<BasePayloadServiceConfig>,
    ) -> eyre::Result<base_node_context::BaseNodeContext> {
        let evm_config = BaseEvmConfig::new(ctx.chain_spec());
        let pool = self.build_pool(ctx, evm_config.clone()).await?;
        let network = self.build_network(ctx, pool.clone()).await?;
        let payload = payload.unwrap_or_else(|| BasePayloadServiceConfig {
            config: base_execution_payload_builder::config::BaseBuilderConfig {
                da_config: self.da_config.clone(),
                gas_limit_config: self.gas_limit_config.clone(),
                manifest_precheck_enabled: self.manifest_precheck_enabled,
                resource_metering: self.resource_metering.clone(),
                rejection_cache: self.rejection_cache.clone(),
                ..Default::default()
            },
            ..Default::default()
        });
        let payload_builder_handle = payload.start(ctx, pool.clone(), evm_config.clone()).await?;
        Ok(base_node_context::BaseNodeContext {
            provider: ctx.provider().clone(),
            task_executor: ctx.task_executor().clone(),
            transaction_pool: pool,
            evm_config,
            network,
            payload_builder_handle,
            consensus: Arc::new(base_execution_evm_blocks::BaseBeaconConsensus::new(
                ctx.chain_spec(),
            )),
        })
    }

    /// Instantiates the [`ProviderFactoryBuilder`] for a Base node.
    ///
    /// # Open a `ProviderFactory` in read-only mode from a datadir
    ///
    /// See also: [`ProviderFactoryBuilder`] and
    /// [`ReadOnlyConfig`](reth_provider::providers::ReadOnlyConfig).
    ///
    /// ```no_run
    /// use base_common_chain_config::BaseChainSpec;
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
    /// use base_common_chain_config::BaseChainSpecBuilder;
    /// use base_node_core::BaseNode;
    /// use reth_provider::providers::ReadOnlyConfig;
    ///
    /// fn demo(runtime: reth_tasks::Runtime) {
    ///     let factory = BaseNode::provider_factory_builder()
    ///         .open_read_only(
    ///             BaseChainSpecBuilder::base_mainnet().build(),
    ///             ReadOnlyConfig::from_datadir("datadir").no_watch(),
    ///             runtime,
    ///         )
    ///         .unwrap();
    /// }
    /// ```
    pub fn provider_factory_builder() -> ProviderFactoryBuilder {
        ProviderFactoryBuilder::default()
    }
}

impl BaseNode {
    /// Builds the Base pool and starts its maintenance and invalidation tasks.
    pub async fn build_pool(
        &self,
        ctx: &BuilderContext,
        evm_config: BaseEvmConfig,
    ) -> eyre::Result<BaseTransactionPool<BlockchainProvider, DiskFileBlobStore>> {
        let ordering = match self.args.txpool_ordering {
            TxpoolOrdering::CoinbaseTip => BaseOrdering::coinbase_tip(),
            TxpoolOrdering::Timestamp => BaseOrdering::timestamp(),
        };
        let max_inflight_delegated_slots = self.args.max_inflight_delegated_slots;
        let guard_limits = GuardLimits {
            signature_limit: self.args.mempool_sender_limit,
            payment_limit: self.args.mempool_payer_limit,
        };
        let blob_store = crate::create_blob_store(ctx)?;
        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .no_eip4844()
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone())
                .map(|validator| {
                    BaseTransactionValidator::new(validator)
                        // In --dev mode we can't require gas fees because we're unable to decode
                        // the L1 block info
                        .require_l1_data_gas_fee(!ctx.config().dev.dev)
                        .with_additional_trusted_delegation_targets(
                            self.args.mempool_trusted_delegation_targets.iter().copied().collect(),
                        )
                });

        let mut final_pool_config = ctx.pool_config();
        final_pool_config.max_inflight_delegated_slot_limit = max_inflight_delegated_slots;

        let transaction_pool = base_execution_txpool::Pool::new(
            validator,
            ordering.clone(),
            blob_store,
            final_pool_config.clone(),
        );
        let transaction_pool =
            BaseTransactionPool::new(transaction_pool, ordering).with_guard_limits(guard_limits);
        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &final_pool_config)?;
        let state_diff_events = BroadcastStream::new(ctx.provider().subscribe_to_canonical_state());
        ctx.task_executor().spawn_critical_task(
            "mempool-invalidation",
            maintain_state_diff_invalidation(transaction_pool.clone(), state_diff_events),
        );

        info!(
            target: "reth::cli",
            max_inflight_delegated_slots = max_inflight_delegated_slots,
            sender_limit = guard_limits.signature_limit,
            payer_limit = guard_limits.payment_limit,
            "Transaction pool initialized"
        );
        debug!(target: "reth::cli", "Spawned txpool maintenance tasks");

        Ok(transaction_pool)
    }
}

impl BaseNode {
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
}

impl BaseDiscoveryConfig {
    /// Creates a new discovery config.
    pub const fn new(disable_discovery_v4: bool) -> Self {
        Self { disable_discovery_v4 }
    }

    /// Returns true if discv4 discovery should be disabled.
    pub const fn should_disable_discv4(&self, discovery: &DiscoveryArgs) -> bool {
        self.disable_discovery_v4
            || discovery.disable_discovery
            || discovery.disable_discv4_discovery
    }

    /// Applies Base discovery settings to the reth network config builder.
    pub fn apply_to_network_builder(
        &self,
        mut builder: NetworkConfigBuilder,
        args: &RethNetworkArgs,
        boot_nodes: impl IntoIterator<Item = NodeRecord>,
        external_addr: Option<IpAddr>,
    ) -> NetworkConfigBuilder {
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

    /// Creates the inner discv5 config with the Base protocol identity.
    pub fn discv5_config(&self, args: &RethNetworkArgs) -> reth_discv5::discv5::Config {
        let mut builder = reth_discv5::discv5::ConfigBuilder::new(Self::discv5_listen_config(args));

        builder.protocol_identity(reth_discv5::discv5::ProtocolIdentity {
            protocol_id: BASE_V0_PROTOCOL_VERSION,
            ..Default::default()
        });

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
            discv5_addr_ipv4.map(|addr| {
                SocketAddrV4::new(
                    addr,
                    args.discovery.discv5_port.unwrap_or_else(|| rlpx_socket.port()),
                )
            }),
            discv5_addr_ipv6.map(|addr| {
                SocketAddrV6::new(
                    addr,
                    args.discovery.discv5_port_ipv6.unwrap_or_else(|| rlpx_socket.port()),
                    0,
                    0,
                )
            }),
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

impl BaseNode {
    /// Returns the [`NetworkConfig`] that contains the settings to launch the p2p network.
    ///
    /// Uses the node’s discovery and networking arguments.
    pub fn network_config(
        &self,
        ctx: &BuilderContext,
    ) -> eyre::Result<NetworkConfig<BlockchainProvider>> {
        let discovery_config = BaseDiscoveryConfig::new(!self.args.discovery_v4);
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
                        .or_else(|| {
                            reth_network_peers::NodeRecord::parse_bootnodes(
                                ctx.chain_spec().config.bootnodes.execution,
                            )
                        })
                        .unwrap_or_default(),
                    external_addr,
                )
            });

        let mut network_config = ctx.build_network_config(network_builder);

        network_config.tx_gossip_disabled = true;

        Ok(network_config)
    }
}

impl BaseNode {
    /// Starts the Base network and its transaction-pool services.
    pub async fn build_network(
        &self,
        ctx: &BuilderContext,
        pool: base_node_context::BaseNodePool<BlockchainProvider>,
    ) -> eyre::Result<NetworkHandle> {
        let network_config = self.network_config(ctx)?;
        let network = NetworkManager::builder(network_config).await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        sync::Arc,
    };

    use reth_discv5::{build_local_enr, discv5::ListenConfig};
    use reth_network::{NetworkConfigBuilder, config::rng_secret_key};
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
        let discovery_config = BaseDiscoveryConfig::new(disable_by_base);

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
        let discovery_config = BaseDiscoveryConfig::new(disable_by_base);

        let network_config = discovery_config
            .apply_to_network_builder(
                NetworkConfigBuilder::with_rng_secret_key(reth_tasks::Runtime::test()),
                &args,
                Vec::<NodeRecord>::new(),
                None,
            )
            .build_with_noop_provider(Arc::new(BaseChainSpec::mainnet()));

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
        let discovery_config = BaseDiscoveryConfig::new(false);

        let network_config = discovery_config
            .apply_to_network_builder(
                NetworkConfigBuilder::with_rng_secret_key(reth_tasks::Runtime::test()),
                &args,
                Vec::<NodeRecord>::new(),
                None,
            )
            .build_with_noop_provider(Arc::new(BaseChainSpec::mainnet()));

        assert_eq!(network_config.discovery_v5_config.is_some(), expected_enabled);
    }

    #[test]
    fn discv5_config_uses_base_protocol_identity() {
        let args = RethNetworkArgs::default();
        let discovery_config = BaseDiscoveryConfig::new(false);

        let discv5_config = discovery_config.discv5_config(&args);

        assert_eq!(discv5_config.protocol_identity.protocol_id, BASE_V0_PROTOCOL_VERSION);
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
        args.discovery.discv5_port = Some(discv5_port);
        args.discovery.discv5_port_ipv6 = Some(discv5_port_ipv6);
        let discovery_config = BaseDiscoveryConfig::new(false);

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
        let discovery_config = BaseDiscoveryConfig::new(false);

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
        let discovery_config = BaseDiscoveryConfig::new(false);

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
