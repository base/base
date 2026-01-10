//! Flashblocks-aware wrapper around [`TestHarness`] that wires in the custom RPC modules.

use std::sync::Arc;

use base_flashtypes::Flashblock;
use derive_more::Deref;
use eyre::Result;

use crate::{
    harness::TestHarness,
    init_silenced_tracing,
    node::{FlashblocksLocalNode, FlashblocksParts, LocalFlashblocksState},
};

/// Helper that exposes [`TestHarness`] conveniences plus Flashblocks helpers.
#[derive(Debug, Deref)]
pub struct FlashblocksHarness {
    #[deref]
    inner: TestHarness,
    parts: FlashblocksParts,
}

impl FlashblocksHarness {
    /// Launch a flashblocks-enabled harness with the default launcher.
    pub async fn new() -> Result<Self> {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::new().await?;
        Self::from_flashblocks_node(flash_node).await
    }

    /// Launch the harness configured for manual canonical progression.
    pub async fn manual_canonical() -> Result<Self> {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::manual_canonical().await?;
        Self::from_flashblocks_node(flash_node).await
    }

    /// Get a handle to the in-memory Flashblocks state backing the harness.
    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.parts.state()
    }

    /// Send a single flashblock through the harness.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    async fn from_flashblocks_node(flash_node: FlashblocksLocalNode) -> Result<Self> {
        let (node, parts) = flash_node.into_parts();
        let inner = TestHarness::from_node(node).await?;
        Ok(Self { inner, parts })
    }
}
