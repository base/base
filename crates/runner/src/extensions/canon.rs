//! Contains the [FlashblocksCanonExtension] which wires up the canonical block
//! subscription for flashblocks on the Base node builder.

use std::sync::Arc;

use base_reth_flashblocks::FlashblocksState;
use reth::providers::CanonStateSubscriptions;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::{
    BaseNodeConfig, FlashblocksConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, OpBuilder},
};

/// Helper struct that wires the Flashblocks canonical subscription into the node builder.
#[derive(Debug, Clone)]
pub struct FlashblocksCanonExtension {
    /// Shared Flashblocks state cache.
    pub cell: FlashblocksCell,
    /// Optional Flashblocks configuration.
    pub config: Option<FlashblocksConfig>,
}

impl FlashblocksCanonExtension {
    /// Create a new Flashblocks canon extension helper.
    pub fn new(config: &BaseNodeConfig) -> Self {
        Self { cell: config.flashblocks_cell.clone(), config: config.flashblocks.clone() }
    }
}

impl BaseNodeExtension for FlashblocksCanonExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let flashblocks = self.config;
        let flashblocks_enabled = flashblocks.is_some();
        let flashblocks_cell = self.cell;

        if !flashblocks_enabled {
            return builder;
        }

        builder.extend_rpc_modules(move |ctx| {
            let fb_config = flashblocks.as_ref().expect("checked above").clone();

            let fb = flashblocks_cell
                .get_or_init(|| {
                    Arc::new(FlashblocksState::new(
                        ctx.provider().clone(),
                        fb_config.max_pending_blocks_depth,
                    ))
                })
                .clone();

            // Subscribe to canonical state notifications
            info!(message = "Starting flashblocks canonical subscription");
            let mut receiver = ctx.provider().subscribe_to_canonical_state();
            tokio::spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(notification) => {
                            let committed = notification.committed();
                            let chain = Arc::unwrap_or_clone(committed);
                            for (_, block) in chain.into_blocks() {
                                fb.on_canonical_block_received(block);
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            warn!(
                                message = "Canonical subscription lagged",
                                missed_notifications = n
                            );
                        }
                        Err(RecvError::Closed) => {
                            warn!(message = "Canonical state subscription ended");
                            break;
                        }
                    }
                }
            });

            Ok(())
        })
    }
}

impl ConfigurableBaseNodeExtension for FlashblocksCanonExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        Ok(Self::new(config))
    }
}
