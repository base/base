//! Flashblocks test harness module.
//!
//! Provides test utilities for flashblocks including:
//! - [`FlashblocksHarness`] - High-level test harness wrapping [`TestHarness`]
//! - [`FlashblocksParts`] - Components for interacting with flashblocks worker tasks
//! - [`FlashblocksTestExtension`] - Node extension for wiring up flashblocks in tests
//! - [`FlashblocksLocalNode`] - Local node wrapper with flashblocks helpers

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use base_client_node::{
    BaseBuilder, BaseNodeExtension,
    test_utils::{
        LocalNode, NODE_STARTUP_DELAY_MS, TestHarness, build_test_genesis, init_silenced_tracing,
    },
};
use base_flashtypes::Flashblock;
use derive_more::Deref;
use eyre::Result;
use futures_util::TryStreamExt as _;
use reth_exex::ExExEvent;
use reth_optimism_chainspec::OpChainSpec;
use reth_provider::CanonStateSubscriptions;
use tokio::sync::{mpsc, oneshot};

use crate::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksReceiver,
    FlashblocksState,
};

/// Components that allow tests to interact with the Flashblocks worker tasks.
#[derive(Clone)]
pub struct FlashblocksParts {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    state: Arc<FlashblocksState>,
}

impl fmt::Debug for FlashblocksParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksParts").finish_non_exhaustive()
    }
}

impl FlashblocksParts {
    /// Clone the shared [`FlashblocksState`] handle.
    pub fn state(&self) -> Arc<FlashblocksState> {
        self.state.clone()
    }

    /// Send a flashblock to the background processor and wait until it is handled.
    pub async fn send(&self, flashblock: Flashblock) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((flashblock, tx)).await.map_err(|err| eyre::eyre!(err))?;
        rx.await.map_err(|err| eyre::eyre!(err))?;
        Ok(())
    }
}

/// Test extension for flashblocks functionality.
///
/// This extension wires up the flashblocks ExEx and RPC modules for testing,
/// with optional control over canonical block processing.
#[derive(Clone, Debug)]
pub struct FlashblocksTestExtension {
    inner: Arc<FlashblocksTestExtensionInner>,
}

struct FlashblocksTestExtensionInner {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    #[allow(clippy::type_complexity)]
    receiver: Arc<Mutex<Option<mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>>>>,
    state: Arc<FlashblocksState>,
    process_canonical: bool,
}

impl fmt::Debug for FlashblocksTestExtensionInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksTestExtensionInner")
            .field("process_canonical", &self.process_canonical)
            .finish_non_exhaustive()
    }
}

impl FlashblocksTestExtension {
    /// Create a new flashblocks test extension.
    ///
    /// If `process_canonical` is true, canonical blocks are automatically processed.
    /// Set to false for tests that need manual control over canonical block timing.
    pub fn new(process_canonical: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let inner = FlashblocksTestExtensionInner {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            state: Arc::new(FlashblocksState::new(5)),
            process_canonical,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Get the flashblocks parts after the node has been launched.
    pub fn parts(&self) -> Result<FlashblocksParts> {
        Ok(FlashblocksParts { sender: self.inner.sender.clone(), state: self.inner.state.clone() })
    }
}

impl BaseNodeExtension for FlashblocksTestExtension {
    fn apply(self: Box<Self>, mut builder: BaseBuilder) -> BaseBuilder {
        let state = self.inner.state.clone();
        let receiver = self.inner.receiver.clone();
        let process_canonical = self.inner.process_canonical;

        let state_for_exex = state.clone();
        let state_for_rpc = state.clone();

        // Install the canon ExEx (directly on inner builder - no accumulation needed)
        builder.builder = builder.builder.install_exex("flashblocks-canon", move |mut ctx| {
            let fb = state_for_exex.clone();
            async move {
                Ok(async move {
                    while let Some(note) = ctx.notifications.try_next().await? {
                        if let Some(committed) = note.committed_chain() {
                            let hash = committed.tip().num_hash();
                            if process_canonical {
                                // Many suites drive canonical updates manually to reproduce race conditions, so
                                // allowing this to be disabled keeps canonical replay deterministic.
                                let chain = Arc::unwrap_or_clone(committed);
                                for (_, block) in chain.into_blocks() {
                                    fb.on_canonical_block_received(block);
                                }
                            }
                            let _ = ctx.events.send(ExExEvent::FinishedHeight(hash));
                        }
                    }
                    Ok(())
                })
            }
        });

        builder.add_rpc_module(move |ctx| {
            let fb = state_for_rpc;
            let provider = ctx.provider().clone();

            // Start the state processor with the provider
            fb.start(provider.clone());

            let mut canon_stream = tokio_stream::wrappers::BroadcastStream::new(
                ctx.provider().subscribe_to_canonical_state(),
            );
            tokio::spawn(async move {
                use tokio_stream::StreamExt;
                while let Some(Ok(notification)) = canon_stream.next().await {
                    provider.canonical_in_memory_state().notify_canon_state(notification);
                }
            });
            let api_ext = EthApiExt::new(
                ctx.registry.eth_api().clone(),
                ctx.registry.eth_handlers().filter.clone(),
                fb.clone(),
            );
            ctx.modules.replace_configured(api_ext.into_rpc())?;

            // Register eth_subscribe subscription endpoint for flashblocks
            // Uses replace_configured since eth_subscribe already exists from reth's standard module
            // Pass eth_api to enable proxying standard subscription types to reth's implementation
            let eth_pubsub = EthPubSub::new(ctx.registry.eth_api().clone(), fb.clone());
            ctx.modules.replace_configured(eth_pubsub.into_rpc())?;

            let fb_for_task = fb.clone();
            let mut receiver = receiver
                .lock()
                .expect("flashblock receiver mutex poisoned")
                .take()
                .expect("flashblock receiver should only be initialized once");
            tokio::spawn(async move {
                while let Some((payload, tx)) = receiver.recv().await {
                    fb_for_task.on_flashblock_received(payload);
                    let _ = tx.send(());
                }
            });

            Ok(())
        })
    }
}

/// Local node wrapper that exposes helpers specific to Flashblocks tests.
pub struct FlashblocksLocalNode {
    node: LocalNode,
    parts: FlashblocksParts,
}

impl fmt::Debug for FlashblocksLocalNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksLocalNode")
            .field("node", &self.node)
            .field("parts", &self.parts)
            .finish()
    }
}

impl FlashblocksLocalNode {
    /// Launch a flashblocks-enabled node using the default configuration.
    pub async fn new() -> Result<Self> {
        Self::with_options(true).await
    }

    /// Builds a flashblocks-enabled node with canonical block streaming disabled so tests can call
    /// `FlashblocksState::on_canonical_block_received` at precise points.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_options(false).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        // Build default chain spec programmatically
        let genesis = build_test_genesis();
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();
        let node = LocalNode::new(vec![Box::new(extension)], chain_spec).await?;
        let parts = parts_source.parts()?;
        Ok(Self { node, parts })
    }

    /// Access the shared Flashblocks state for assertions or manual driving.
    pub fn flashblocks_state(&self) -> Arc<FlashblocksState> {
        self.parts.state()
    }

    /// Send a flashblock through the background processor and await completion.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    /// Split the wrapper into the underlying node plus flashblocks parts.
    pub fn into_parts(self) -> (LocalNode, FlashblocksParts) {
        (self.node, self.parts)
    }
}

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
    pub fn flashblocks_state(&self) -> Arc<FlashblocksState> {
        self.parts.state()
    }

    /// Send a single flashblock through the harness.
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        self.parts.send(flashblock).await
    }

    async fn with_options(process_canonical: bool) -> Result<Self> {
        init_silenced_tracing();

        // Build default chain spec programmatically
        let genesis = build_test_genesis();
        let chain_spec = Arc::new(OpChainSpec::from_genesis(genesis));

        // Create the extension and keep a reference to get parts after launch
        let extension = FlashblocksTestExtension::new(process_canonical);
        let parts_source = extension.clone();

        // Launch the node with the flashblocks extension
        let node = LocalNode::new(vec![Box::new(extension)], chain_spec).await?;
        let engine = node.engine_api()?;

        tokio::time::sleep(Duration::from_millis(NODE_STARTUP_DELAY_MS)).await;

        // Get the parts from the extension after node launch
        let parts = parts_source.parts()?;

        // Create harness by building it directly (avoiding TestHarnessBuilder since we already have node)
        let inner = TestHarness::from_parts(node, engine);

        Ok(Self { inner, parts })
    }
}
