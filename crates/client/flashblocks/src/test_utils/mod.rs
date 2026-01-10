//! Test utilities for flashblocks integration tests.
//!
//! This module provides test harnesses and helpers for testing flashblocks functionality.
//! It is gated behind the `test-utils` feature flag.

mod harness;
pub use harness::FlashblocksHarness;

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use base_reth_test_utils::{
    LocalNode, LocalNodeProvider, OpAddOns, OpBuilder, default_launcher, init_silenced_tracing,
};
use eyre::Result;
use futures_util::Future;
use once_cell::sync::OnceCell;
use reth::builder::NodeHandle;
use reth_e2e_test_utils::Adapter;
use reth_exex::ExExEvent;
use reth_optimism_node::OpNode;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;

use crate::{
    EthApiExt, EthApiOverrideServer, EthPubSub, EthPubSubApiServer, FlashblocksReceiver,
    FlashblocksState,
};

use base_flashtypes::Flashblock;
use reth_provider::CanonStateSubscriptions;

/// Convenience alias for the Flashblocks state backing the local node.
pub type LocalFlashblocksState = FlashblocksState<LocalNodeProvider>;

/// Components that allow tests to interact with the Flashblocks worker tasks.
#[derive(Clone)]
pub struct FlashblocksParts {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    state: Arc<LocalFlashblocksState>,
}

impl fmt::Debug for FlashblocksParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashblocksParts").finish_non_exhaustive()
    }
}

impl FlashblocksParts {
    /// Clone the shared [`FlashblocksState`] handle.
    pub fn state(&self) -> Arc<LocalFlashblocksState> {
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

#[derive(Clone)]
struct FlashblocksNodeExtensions {
    inner: Arc<FlashblocksNodeExtensionsInner>,
}

struct FlashblocksNodeExtensionsInner {
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
    #[allow(clippy::type_complexity)]
    receiver: Arc<Mutex<Option<mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>>>>,
    fb_cell: Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    process_canonical: bool,
}

impl FlashblocksNodeExtensions {
    fn new(process_canonical: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<(Flashblock, oneshot::Sender<()>)>(100);
        let inner = FlashblocksNodeExtensionsInner {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            fb_cell: Arc::new(OnceCell::new()),
            process_canonical,
        };
        Self { inner: Arc::new(inner) }
    }

    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let fb_cell = self.inner.fb_cell.clone();
        let receiver = self.inner.receiver.clone();
        let process_canonical = self.inner.process_canonical;

        let fb_cell_for_exex = fb_cell.clone();

        builder
            .install_exex("flashblocks-canon", move |mut ctx| {
                let fb_cell = fb_cell_for_exex.clone();
                let process_canonical = process_canonical;
                async move {
                    let provider = ctx.provider().clone();
                    let fb = init_flashblocks_state(&fb_cell, &provider);
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
            })
            .extend_rpc_modules(move |ctx| {
                let fb_cell = fb_cell.clone();
                let provider = ctx.provider().clone();
                let fb = init_flashblocks_state(&fb_cell, &provider);

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

    fn wrap_launcher<L, LRet>(&self, launcher: L) -> impl FnOnce(OpBuilder) -> LRet
    where
        L: FnOnce(OpBuilder) -> LRet,
    {
        let extensions = self.clone();
        move |builder| {
            let builder = extensions.apply(builder);
            launcher(builder)
        }
    }

    fn parts(&self) -> Result<FlashblocksParts> {
        let state = self.inner.fb_cell.get().ok_or_else(|| {
            eyre::eyre!("FlashblocksState should be initialized during node launch")
        })?;
        Ok(FlashblocksParts { sender: self.inner.sender.clone(), state: state.clone() })
    }
}

fn init_flashblocks_state(
    cell: &Arc<OnceCell<Arc<LocalFlashblocksState>>>,
    provider: &LocalNodeProvider,
) -> Arc<LocalFlashblocksState> {
    cell.get_or_init(|| {
        let fb = Arc::new(FlashblocksState::new(provider.clone(), 5));
        fb.start();
        fb
    })
    .clone()
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
    /// Launch a flashblocks-enabled node using the default launcher.
    pub async fn new() -> Result<Self> {
        Self::with_launcher(default_launcher).await
    }

    /// Builds a flashblocks-enabled node with canonical block streaming disabled so tests can call
    /// `FlashblocksState::on_canonical_block_received` at precise points.
    pub async fn manual_canonical() -> Result<Self> {
        Self::with_manual_canonical_launcher(default_launcher).await
    }

    /// Launch a flashblocks-enabled node with a custom launcher and canonical processing enabled.
    pub async fn with_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        Self::with_launcher_inner(launcher, true).await
    }

    /// Same as [`Self::with_launcher`] but leaves canonical processing to the caller.
    pub async fn with_manual_canonical_launcher<L, LRet>(launcher: L) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        Self::with_launcher_inner(launcher, false).await
    }

    async fn with_launcher_inner<L, LRet>(launcher: L, process_canonical: bool) -> Result<Self>
    where
        L: FnOnce(OpBuilder) -> LRet,
        LRet: Future<Output = eyre::Result<NodeHandle<Adapter<OpNode>, OpAddOns>>>,
    {
        init_silenced_tracing();
        let extensions = FlashblocksNodeExtensions::new(process_canonical);
        let wrapped_launcher = extensions.wrap_launcher(launcher);
        let node = LocalNode::new(wrapped_launcher).await?;

        let parts = extensions.parts()?;
        Ok(Self { node, parts })
    }

    /// Access the shared Flashblocks state for assertions or manual driving.
    pub fn flashblocks_state(&self) -> Arc<LocalFlashblocksState> {
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

    /// Borrow the underlying [`LocalNode`].
    pub fn as_node(&self) -> &LocalNode {
        &self.node
    }
}
