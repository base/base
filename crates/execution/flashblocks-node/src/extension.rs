//! Contains the [`FlashblocksExtension`] which wires up the flashblocks feature
//! (canonical block subscription and RPC surface) on the Base node builder.

use std::sync::Arc;

use base_flashblocks::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksConfig,
    FlashblocksSubscriber,
};
use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};
use reth_chain_state::CanonStateSubscriptions;
use reth_provider::{BlockNumReader, BlockReader, TransactionVariant};
use tokio_stream::{
    StreamExt,
    wrappers::{BroadcastStream, errors::BroadcastStreamRecvError},
};
use tracing::{info, warn};

/// Helper struct that wires the Flashblocks feature (canonical subscription and RPC) into the node builder.
#[derive(Debug)]
pub struct FlashblocksExtension {
    /// Optional Flashblocks configuration (includes state).
    config: Option<FlashblocksConfig>,
}

impl FlashblocksExtension {
    /// Create a new Flashblocks extension helper.
    pub const fn new(config: Option<FlashblocksConfig>) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for FlashblocksExtension {
    /// Applies the extension to the supplied hooks.
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let Some(cfg) = self.config else {
            info!(message = "flashblocks integration is disabled");
            return hooks;
        };

        let state = cfg.state;
        let mut subscriber = FlashblocksSubscriber::new(
            Arc::clone(&state),
            cfg.websocket_url,
            cfg.subscriber_ping_interval,
        );

        let state_for_canonical = Arc::clone(&state);
        let state_for_rpc = Arc::clone(&state);
        let state_for_start = state;

        // Start state processor, subscriber, and canonical subscription after node is started
        let hooks = hooks.add_node_started_hook(move |ctx| {
            info!(message = "Starting Flashblocks state processor");
            state_for_start.start(ctx.provider().clone());
            subscriber.start();

            let provider = ctx.provider().clone();
            let mut canonical_stream =
                BroadcastStream::new(provider.subscribe_to_canonical_state());
            tokio::spawn(async move {
                while let Some(result) = canonical_stream.next().await {
                    match result {
                        Ok(notification) => {
                            let committed = notification.committed();
                            for block in committed.blocks_iter() {
                                state_for_canonical
                                    .on_canonical_block_received(block.as_ref().clone());
                            }
                        }
                        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
                            warn!(
                                skipped,
                                "canonical state subscription lagged; resynchronizing from provider"
                            );
                            let latest = provider.best_block_number().ok().and_then(|number| {
                                provider
                                    .recovered_block(number.into(), TransactionVariant::WithHash)
                                    .ok()
                                    .flatten()
                            });
                            if let Some(block) = latest {
                                state_for_canonical.on_canonical_block_received(block);
                            } else {
                                warn!("canonical state subscription could not load provider tip");
                            }
                        }
                    }
                }
                warn!("canonical state subscription closed");
            });

            Ok(())
        });

        // Extend with RPC modules
        hooks.add_rpc_module(move |ctx| {
            info!(message = "Starting Flashblocks RPC");

            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                Arc::clone(&state_for_rpc),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register the eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(
                ctx.registry.eth_api().clone(),
                ctx.node().task_executor.clone(),
                state_for_rpc,
            );
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

            Ok(())
        })
    }
}

impl FromExtensionConfig for FlashblocksExtension {
    type Config = Option<FlashblocksConfig>;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}
