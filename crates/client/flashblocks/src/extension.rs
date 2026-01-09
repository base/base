//! Contains the [`FlashblocksCanonExtension`] which wires up the `flashblocks-canon`
//! execution extension on the Base node builder.

use std::sync::Arc;

use base_primitives::{
    BaseNodeExtension, ConfigurableBaseNodeExtension, FlashblocksCell, FlashblocksConfig,
    OpBuilder, OpProvider,
};
use futures_util::TryStreamExt;
use reth_exex::ExExEvent;

use crate::FlashblocksState;

/// Helper struct that wires the Flashblocks canon ExEx into the node builder.
#[derive(Debug, Clone)]
pub struct FlashblocksCanonExtension {
    /// Shared Flashblocks state cache.
    pub cell: FlashblocksCell<FlashblocksState<OpProvider>>,
    /// Optional Flashblocks configuration.
    pub config: Option<FlashblocksConfig>,
}

impl FlashblocksCanonExtension {
    /// Create a new Flashblocks canon extension helper.
    pub const fn new(
        cell: FlashblocksCell<FlashblocksState<OpProvider>>,
        config: Option<FlashblocksConfig>,
    ) -> Self {
        Self { cell, config }
    }
}

impl BaseNodeExtension for FlashblocksCanonExtension {
    /// Applies the extension to the supplied builder.
    fn apply(self: Box<Self>, builder: OpBuilder) -> OpBuilder {
        let flashblocks = self.config;
        let flashblocks_enabled = flashblocks.is_some();
        let flashblocks_cell = self.cell;

        builder.install_exex_if(flashblocks_enabled, "flashblocks-canon", move |mut ctx| {
            let flashblocks_cell = flashblocks_cell.clone();
            async move {
                let fb_config =
                    flashblocks.as_ref().expect("flashblocks config checked above").clone();
                let fb = flashblocks_cell
                    .get_or_init(|| {
                        Arc::new(FlashblocksState::new(
                            ctx.provider().clone(),
                            fb_config.max_pending_blocks_depth,
                        ))
                    })
                    .clone();

                Ok(async move {
                    while let Some(note) = ctx.notifications.try_next().await? {
                        if let Some(committed) = note.committed_chain() {
                            let tip = committed.tip().num_hash();
                            let chain = Arc::unwrap_or_clone(committed);
                            for (_, block) in chain.into_blocks() {
                                fb.on_canonical_block_received(block);
                            }
                            let _ = ctx.events.send(ExExEvent::FinishedHeight(tip));
                        }
                    }
                    Ok(())
                })
            }
        })
    }
}

/// Configuration trait for [`FlashblocksCanonExtension`].
///
/// Types implementing this trait can be used to construct a [`FlashblocksCanonExtension`].
pub trait FlashblocksCanonConfig {
    /// Returns the shared flashblocks cell.
    fn flashblocks_cell(&self) -> &FlashblocksCell<FlashblocksState<OpProvider>>;
    /// Returns the flashblocks configuration if enabled.
    fn flashblocks(&self) -> Option<&FlashblocksConfig>;
}

impl<C: FlashblocksCanonConfig> ConfigurableBaseNodeExtension<C> for FlashblocksCanonExtension {
    fn build(config: &C) -> eyre::Result<Self> {
        Ok(Self::new(config.flashblocks_cell().clone(), config.flashblocks().cloned()))
    }
}
