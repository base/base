//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use base_node_runner::{BaseNodeAdapter, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use futures::TryStreamExt;
use reth_exex::{ExExContext, ExExEvent, ExExNotificationsStream};
use tracing::{debug, info};

/// `ExEx` run loop: drain canonical-chain notifications, report `FinishedHeight`.
pub async fn run_mev_emitter_exex(mut ctx: ExExContext<BaseNodeAdapter>) -> eyre::Result<()> {
    info!(target: "base::mev_emitter", "mev-emitter ExEx started");
    ctx.notifications.set_without_head();
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(committed) = notification.committed_chain() {
            let tip = committed.tip().num_hash();
            debug!(
                target: "base::mev_emitter",
                number = tip.number,
                hash = ?tip.hash,
                blocks = committed.blocks().len(),
                "chain committed",
            );
            // C-2 attaches the revm Inspector here to capture per-tx (account,
            // token) net balance deltas and emits StateDiffEvent over the C-4
            // transport. The FinishedHeight report lets reth prune behind us.
            ctx.events.send(ExExEvent::FinishedHeight(tip))?;
        }
    }
    Ok(())
}

/// Node extension that installs the [`run_mev_emitter_exex`] `ExEx` via
/// [`NodeHooks::install_exex`]. Register with `BaseNodeRunner::install_ext`.
#[derive(Debug)]
pub struct MevEmitterExtension;

impl FromExtensionConfig for MevEmitterExtension {
    type Config = ();

    fn from_config(_config: Self::Config) -> Self {
        Self
    }
}

impl BaseNodeExtension for MevEmitterExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.install_exex("mev-emitter", move |ctx| async move {
            Ok(run_mev_emitter_exex(ctx))
        })
    }
}
