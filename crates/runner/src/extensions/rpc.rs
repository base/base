//! Contains the [BaseRpcExtension] which wires up the custom Base RPC modules on the node builder.

use std::sync::Arc;

use alloy_primitives::{B256, U256, keccak256};
use base_flashtypes::Flashblock;
use base_reth_flashblocks::{FlashblocksReceiver, FlashblocksState, FlashblocksSubscriber};
use base_reth_rpc::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblockInclusion,
    KafkaBundleConsumer, KafkaBundleConsumerConfig, MeteredTransaction, MeteringApiImpl,
    MeteringApiServer, MeteringCache, PriorityFeeEstimator, ResourceAnnotator, ResourceLimits,
    TransactionStatusApiImpl, TransactionStatusApiServer,
};
use parking_lot::RwLock;
use rdkafka::ClientConfig;
use reth_optimism_payload_builder::config::OpDAConfig;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    BaseNodeConfig, FlashblocksConfig, MeteringConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, OpBuilder},
};

/// Runtime state for the metering pipeline.
#[derive(Clone)]
struct MeteringRuntime {
    /// Priority fee estimator.
    estimator: Arc<PriorityFeeEstimator>,
    /// Sender for metered transactions from Kafka.
    tx_sender: mpsc::UnboundedSender<MeteredTransaction>,
    /// Sender for flashblock inclusions.
    flashblock_sender: mpsc::UnboundedSender<FlashblockInclusion>,
}

/// Composite receiver that forwards flashblocks to both FlashblocksState and the metering pipeline.
struct CompositeFlashblocksReceiver<Client> {
    state: Arc<FlashblocksState<Client>>,
    /// Optional channel for the metering pipeline; flashblocks RPC still needs the stream even
    /// when metering is disabled, so we only forward inclusions if a sender is provided.
    metering_sender: Option<mpsc::UnboundedSender<FlashblockInclusion>>,
}

impl<Client> CompositeFlashblocksReceiver<Client> {
    const fn new(
        state: Arc<FlashblocksState<Client>>,
        metering_sender: Option<mpsc::UnboundedSender<FlashblockInclusion>>,
    ) -> Self {
        Self { state, metering_sender }
    }
}

impl<Client> FlashblocksReceiver for CompositeFlashblocksReceiver<Client>
where
    FlashblocksState<Client>: FlashblocksReceiver,
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        // Forward to the state first
        self.state.on_flashblock_received(flashblock.clone());

        // Then forward to metering if enabled
        let Some(sender) = &self.metering_sender else {
            return;
        };
        let Some(inclusion) = flashblock_inclusion_from_flashblock(&flashblock) else {
            return;
        };

        if sender.send(inclusion).is_err() {
            warn!(
                target: "metering::flashblocks",
                "Failed to forward flashblock inclusion to metering"
            );
        }
    }
}

/// Converts a flashblock to a FlashblockInclusion for the metering pipeline.
fn flashblock_inclusion_from_flashblock(flashblock: &Flashblock) -> Option<FlashblockInclusion> {
    if flashblock.diff.transactions.is_empty() {
        return None;
    }

    let ordered_tx_hashes: Vec<B256> = flashblock.diff.transactions.iter().map(keccak256).collect();

    Some(FlashblockInclusion {
        block_number: flashblock.metadata.block_number,
        flashblock_index: flashblock.index,
        ordered_tx_hashes,
    })
}

/// Loads Kafka configuration from a properties file.
fn load_kafka_config_from_file(
    path: &str,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let mut props = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            props.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(props)
}

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
    /// Shared DA config for dynamic updates via `miner_setMaxDASize`.
    pub da_config: OpDAConfig,
}

impl BaseRpcExtension {
    /// Creates a new RPC extension helper.
    pub fn new(config: &BaseNodeConfig) -> Self {
        Self {
            flashblocks_cell: config.flashblocks_cell.clone(),
            flashblocks: config.flashblocks.clone(),
            metering: config.metering.clone(),
            sequencer_rpc: config.rollup_args.sequencer.clone(),
            da_config: config.da_config.clone(),
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
        let da_config = self.da_config.clone();

        builder.extend_rpc_modules(move |ctx| {
            // Warn if metering is enabled but Kafka is not configured
            if metering.enabled && metering.kafka.is_none() {
                warn!(
                    message = "Metering enabled but Kafka not configured",
                    help = "Priority fee estimation requires --metering-kafka-properties-file"
                );
            }

            // Set up metering runtime if enabled with Kafka
            let metering_runtime = if metering.enabled && metering.kafka.is_some() {
                info!(message = "Starting Metering RPC with priority fee estimation");

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
                    cache.clone(),
                    metering.priority_fee_percentile,
                    limits,
                    default_fee,
                    Some(da_config.clone()),
                ));

                // Create channels for the annotator
                let (tx_sender, tx_receiver) = mpsc::unbounded_channel::<MeteredTransaction>();
                let (flashblock_sender, flashblock_receiver) =
                    mpsc::unbounded_channel::<FlashblockInclusion>();

                // Spawn the resource annotator
                tokio::spawn(async move {
                    ResourceAnnotator::new(cache, tx_receiver, flashblock_receiver).run().await;
                });

                Some(MeteringRuntime { estimator, tx_sender, flashblock_sender })
            } else {
                None
            };

            // Spawn Kafka consumer if configured
            if let (Some(runtime), Some(kafka_cfg)) = (&metering_runtime, &metering.kafka) {
                info!(
                    message = "Starting Kafka consumer for metering",
                    properties_file = %kafka_cfg.properties_file,
                    topic = %kafka_cfg.topic
                );

                // Load all rdkafka settings from the properties file
                let props = match load_kafka_config_from_file(&kafka_cfg.properties_file) {
                    Ok(props) => props,
                    Err(err) => {
                        error!(
                            target: "metering::kafka",
                            file = %kafka_cfg.properties_file,
                            %err,
                            "Failed to load Kafka properties file"
                        );
                        return Ok(());
                    }
                };

                let mut client_config = ClientConfig::new();
                for (key, value) in props {
                    client_config.set(key, value);
                }

                // Apply CLI override for group.id if specified
                if let Some(group_id) = &kafka_cfg.group_id_override {
                    client_config.set("group.id", group_id);
                }

                let tx_sender = runtime.tx_sender.clone();
                let topic = kafka_cfg.topic.clone();
                tokio::spawn(async move {
                    let config = KafkaBundleConsumerConfig { client_config, topic };

                    match KafkaBundleConsumer::new(config, tx_sender) {
                        Ok(consumer) => consumer.run().await,
                        Err(err) => error!(
                            target: "metering::kafka",
                            %err,
                            "Failed to initialize Kafka consumer"
                        ),
                    }
                });
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

                // Create composite receiver that forwards to both flashblocks state and metering
                let metering_sender =
                    metering_runtime.as_ref().map(|rt| rt.flashblock_sender.clone());
                let receiver =
                    Arc::new(CompositeFlashblocksReceiver::new(fb.clone(), metering_sender));

                let mut flashblocks_client = FlashblocksSubscriber::new(receiver, ws_url);
                flashblocks_client.start();

                // Metering API requires flashblocks state to access pending blocks for bundle simulation
                if metering.enabled {
                    info!(message = "Starting Metering RPC");
                    let metering_api = metering_runtime.as_ref().map_or_else(
                        || MeteringApiImpl::new(ctx.provider().clone(), fb.clone()),
                        |rt| {
                            MeteringApiImpl::with_estimator(
                                ctx.provider().clone(),
                                fb.clone(),
                                rt.estimator.clone(),
                            )
                        },
                    );
                    ctx.modules.merge_configured(metering_api.into_rpc())?;
                }

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
                if metering.enabled {
                    info!(message = "Metering RPC requires flashblocks to be enabled - skipping");
                }
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
