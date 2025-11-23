use std::{ops::Deref, sync::Arc};

use base_reth_flashblocks_rpc::subscription::Flashblock;
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

pub struct FlashblocksHarness {
    inner: TestHarness,
    parts: FlashblocksParts,
}

impl FlashblocksHarness {
    pub async fn new() -> Result<Self> {
        Self::with_launcher(default_launcher).await
    }

    pub async fn manual_canonical() -> Result<Self> {
        Self::manual_canonical_with_launcher(default_launcher).await
    }

    pub async fn with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_launcher(launcher).await?;
        Self::from_flashblocks_node(flash_node).await
    }

    pub async fn manual_canonical_with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let flash_node = FlashblocksLocalNode::with_manual_canonical_launcher(launcher).await?;
        Self::from_flashblocks_node(flash_node).await
    }

    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.parts.state()
    }

    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    pub async fn send_flashblocks<I>(&self, flashblocks: I) -> Result<()>
    where
        I: IntoIterator<Item = Flashblock>,
    {
        for flashblock in flashblocks {
            self.send_flashblock(flashblock).await?;
        }
        Ok(())
    }

    pub fn into_inner(self) -> TestHarness {
        self.inner
    }

    async fn from_flashblocks_node(flash_node: FlashblocksLocalNode) -> Result<Self> {
        let (node, parts) = flash_node.into_parts();
        let inner = TestHarness::from_node(node).await?;
        Ok(Self { inner, parts })
    }
}

impl Deref for FlashblocksHarness {
    type Target = TestHarness;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
