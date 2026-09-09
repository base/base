use base_execution_payload_builder::config::{BaseDAConfig, GasLimitConfig};
use base_execution_rpc::BaseEthApiBuilder;

use crate::RpcAddOns;

/// Base public RPC services and shared payload-builder settings.
#[derive(Debug)]
pub struct BaseAddOns {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns,
    /// Data availability configuration for the payload builder.
    pub da_config: BaseDAConfig,
    /// Gas limit configuration for the payload builder.
    pub gas_limit_config: GasLimitConfig,
}

impl BaseAddOns {
    /// Creates a new instance from components.
    pub const fn new(
        rpc_add_ons: RpcAddOns,
        da_config: BaseDAConfig,
        gas_limit_config: GasLimitConfig,
    ) -> Self {
        Self { rpc_add_ons, da_config, gas_limit_config }
    }
}

impl Default for BaseAddOns {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl BaseAddOns {
    /// Build a [`BaseAddOns`] using [`BaseAddOnsBuilder`].
    pub fn builder() -> BaseAddOnsBuilder {
        BaseAddOnsBuilder::default()
    }
}

impl BaseAddOns {}

impl BaseAddOns {
    /// Launches public RPC with Base execution and miner methods.
    pub async fn launch_add_ons(
        self,
        ctx: base_node_context::AddOnsContext<'_>,
        services: &crate::PreparedNodeServices,
    ) -> eyre::Result<crate::BaseNodeRpcHandle> {
        self.rpc_add_ons.launch_add_ons(ctx, self.da_config, self.gas_limit_config, services).await
    }
}

/// A regular Base EVM and executor builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BaseAddOnsBuilder {
    /// Sequencer client, configured to forward submitted transactions to sequencer of the given
    /// Base network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Data availability configuration for the payload builder.
    da_config: Option<BaseDAConfig>,
    /// Gas limit configuration for the payload builder.
    gas_limit_config: Option<GasLimitConfig>,
    /// Minimum suggested priority fee (tip)
    min_suggested_priority_fee: u64,
    /// Optional tokio runtime to use for the RPC server.
    tokio_runtime: Option<tokio::runtime::Handle>,
}

impl Default for BaseAddOnsBuilder {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            da_config: None,
            gas_limit_config: None,
            min_suggested_priority_fee: 1_000_000,
            tokio_runtime: None,
        }
    }
}

impl BaseAddOnsBuilder {
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
}

impl BaseAddOnsBuilder {
    /// Builds an instance of [`BaseAddOns`].
    pub fn build(self) -> BaseAddOns {
        let Self {
            sequencer_url,
            sequencer_headers,
            da_config,
            gas_limit_config,
            min_suggested_priority_fee,
            tokio_runtime,
            ..
        } = self;

        BaseAddOns::new(
            RpcAddOns::new(
                BaseEthApiBuilder::default()
                    .with_sequencer(sequencer_url)
                    .with_sequencer_headers(sequencer_headers)
                    .with_min_suggested_priority_fee(min_suggested_priority_fee),
            )
            .with_tokio_runtime(tokio_runtime),
            da_config.unwrap_or_default(),
            gas_limit_config.unwrap_or_default(),
        )
    }
}
