//! Flashblocks-aware wrapper around [`TestHarness`] that wires in the custom RPC modules.

use std::sync::Arc;

use base_reth_flashblocks::Flashblock;
use derive_more::Deref;
use eyre::Result;
use futures_util::Future;
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_optimism_node::OpNode;

use crate::{
    harness::TestHarness,
    node::{
        FlashblocksLocalNode, FlashblocksParts, LocalFlashblocksState, OpAddOns, OpBuilder,
        default_launcher,
    },
    tracing::init_silenced_tracing,
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
        Self::with_launcher(default_launcher).await
    }

    /// Launch the harness configured for manual canonical progression.
    pub async fn manual_canonical() -> Result<Self> {
        Self::manual_canonical_with_launcher(default_launcher).await
    }

    /// Launch the harness using a custom node launcher.
    pub async fn with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_launcher(launcher).await?;
        Self::from_flashblocks_node(flash_node).await
    }

    /// Launch the harness with a custom launcher while disabling automatic canonical processing.
    pub async fn manual_canonical_with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_manual_canonical_launcher(launcher).await?;
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

    /// Send a batch of flashblocks sequentially, awaiting each confirmation.
    pub async fn send_flashblocks<I>(&self, flashblocks: I) -> Result<()>
    where
        I: IntoIterator<Item = Flashblock>,
    {
        for flashblock in flashblocks {
            self.send_flashblock(flashblock).await?;
        }
        Ok(())
    }

    /// Consume the flashblocks harness and return the underlying [`TestHarness`].
    pub fn into_inner(self) -> TestHarness {
        self.inner
    }

    async fn from_flashblocks_node(flash_node: FlashblocksLocalNode) -> Result<Self> {
        let (node, parts) = flash_node.into_parts();
        let inner = TestHarness::from_node(node).await?;
        Ok(Self { inner, parts })
    }
}
