//! Flashblocks-aware wrapper around [`TestHarness`] that wires in the custom RPC modules.

use std::{sync::Arc, time::Duration};

use alloy_genesis::Genesis;
use base_flashtypes::Flashblock;
use derive_more::Deref;
use eyre::Result;
use reth_optimism_chainspec::OpChainSpec;
use tokio::time::sleep;

use crate::{
    NODE_STARTUP_DELAY_MS,
    harness::TestHarness,
    init_silenced_tracing,
    node::{FlashblocksParts, FlashblocksTestExtension, LocalFlashblocksState, LocalNode},
};

/// Helper that exposes [`TestHarness`] conveniences plus Flashblocks helpers.
#[derive(Debug, Deref)]
pub struct FlashblocksHarness {
    #[deref]
    inner: TestHarness,
    parts: FlashblocksParts,
}

impl FlashblocksHarness {
    /// Launch a flashblocks-enabled harness with automatic canonical processing.
    pub async fn new() -> Result<Self> {
        Self::with_options(true).await
    }

    /// Launch the harness configured for manual canonical progression.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_options(false).await
    }

    /// Get a handle to the in-memory Flashblocks state backing the harness.
    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
        self.parts.state()
    }

    /// Send a single flashblock through the harness.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        init_silenced_tracing();

        // Load default chain spec
        let genesis: Genesis = serde_json::from_str(include_str!("../assets/genesis.json"))?;
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        // Create the extension and keep a reference to get parts after launch
        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();

        // Launch the node with the flashblocks extension
        let node = LocalNode::new(vec![Box::new(extension)], chain_spec).await?;
        let engine = node.engine_api()?;

        sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

        // Get the parts from the extension after node launch
        let parts = parts_source.parts()?;

        // Create harness by building it directly (avoiding TestHarnessBuilder since we already have node)
        let inner = TestHarness::from_parts(node, engine);

        Ok(Self { inner, parts })
    }
}
