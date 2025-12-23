//! Contains the [BaseRpcExtension] which wires up the custom Base RPC modules on the node builder.

use std::sync::Arc;

use alloy_primitives::U256;
use base_reth_flashblocks::{FlashblocksState, FlashblocksSubscriber};
use base_reth_rpc::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, MeteringApiImpl,
    MeteringApiServer, MeteringCache, PriorityFeeEstimator, ResourceLimits,
    TransactionStatusApiImpl, TransactionStatusApiServer,
};
use parking_lot::RwLock;
use tracing::info;
use url::Url;

use crate::{
    BaseNodeConfig, FlashblocksConfig, MeteringConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, OpBuilder},
};

/// Helper struct that wires the custom RPC modules into the node builder.
#[derive(Debug, Clone)]
pub struct BaseRpcExtension {
    /// Shared Flashblocks state cache.
    pub flashblocks_cell: FlashblocksCell,
    /// Optional Flashblocks configuration.
    pub flashblocks: Option<FlashblocksConfig>,
    /// Full metering configuration.
    pub metering: MeteringConfig,
    /// Sequencer RPC endpoint for transaction status proxying.
    pub sequencer_rpc: Option<String>,
}

impl BaseRpcExtension {
    /// Creates a new RPC extension helper.
    pub fn new(config: &BaseNodeConfig) -> Self {
        Self {
            flashblocks_cell: config.flashblocks_cell.clone(),
            flashblocks: config.flashblocks.clone(),
            metering: config.metering.clone(),
            sequencer_rpc: config.rollup_args.sequencer.clone(),
        }
    }
}

impl BaseNodeExtension for BaseRpcExtension {
    /// Applies the extension to the supplied builder.
    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let flashblocks_cell = self.flashblocks_cell.clone();
        let flashblocks = self.flashblocks.clone();
        let metering = self.metering.clone();
        let sequencer_rpc = self.sequencer_rpc.clone();

        builder.extend_rpc_modules(move |ctx| {
            if metering.enabled {
                info!(message = "Starting Metering RPC");

                // Create priority fee estimator if configured
                let estimator = if metering.kafka.is_some() {
                    info!(message = "Enabling priority fee estimation");
                    let cache = Arc::new(RwLock::new(MeteringCache::new(metering.cache_size)));
                    let limits = ResourceLimits {
                        gas_used: Some(metering.resource_limits.gas_limit),
                        execution_time_us: Some(metering.resource_limits.execution_time_us as u128),
                        state_root_time_us: metering
                            .resource_limits
                            .state_root_time_us
                            .map(|v| v as u128),
                        data_availability_bytes: Some(metering.resource_limits.da_bytes),
                    };
                    let default_fee = U256::from(metering.uncongested_priority_fee);
                    let estimator = Arc::new(PriorityFeeEstimator::new(
                        cache,
                        metering.priority_fee_percentile,
                        limits,
                        default_fee,
                        None, // Dynamic DA config not wired yet
                    ));
                    Some(estimator)
                } else {
                    None
                };

                let metering_api = estimator.map_or_else(
                    || MeteringApiImpl::new(ctx.provider().clone()),
                    |est| MeteringApiImpl::with_estimator(ctx.provider().clone(), est),
                );
                ctx.modules.merge_configured(metering_api.into_rpc())?;
            }

            let proxy_api =
                TransactionStatusApiImpl::new(sequencer_rpc.clone(), ctx.pool().clone())
                    .expect("Failed to create transaction status proxy");
            ctx.modules.merge_configured(proxy_api.into_rpc())?;

            if let Some(cfg) = flashblocks.clone() {
                info!(message = "Starting Flashblocks");

                let ws_url = Url::parse(cfg.websocket_url.as_str())?;
                let fb = flashblocks_cell
                    .get_or_init(|| {
                        Arc::new(FlashblocksState::new(
                            ctx.provider().clone(),
                            cfg.max_pending_blocks_depth,
                        ))
                    })
                    .clone();
                fb.start();

                let mut flashblocks_client = FlashblocksSubscriber::new(fb.clone(), ws_url);
                flashblocks_client.start();

                let api_ext = EthApiExt::new(
                    ctx.registry.eth_api().clone(),
                    ctx.registry.eth_handlers().filter.clone(),
                    fb.clone(),
                );
                ctx.modules.replace_configured(api_ext.into_rpc())?;

                // Register the eth_subscribe subscription endpoint for flashblocks
                // Uses replace_configured since eth_subscribe already exists from reth's standard
                // module Pass eth_api to enable proxying standard subscription types to
                // reth's implementation
                let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), fb);
                ctx.modules.replace_configured(eth_pubsub.into_rpc())?;
            } else {
                info!(message = "flashblocks integration is disabled");
            }

            Ok(())
        })
    }
}

impl ConfigurableBaseNodeExtension for BaseRpcExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        Ok(Self::new(config))
    }
}
