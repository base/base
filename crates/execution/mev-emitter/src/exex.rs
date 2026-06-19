//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use std::sync::Arc;
use std::time::Duration;

use alloy_evm::Evm;
use base_execution_evm::BaseEvmConfig;
use base_flashblocks::FlashblocksSubscriber;
use base_node_runner::{BaseNodeAdapter, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use futures::TryStreamExt;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_exex::{ExExContext, ExExEvent, ExExNotificationsStream};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use revm::DatabaseCommit;
use tracing::{debug, info, warn};

use crate::flashblocks::{EmitterFlashblocksReceiver, FlashblockIndex};

/// Margin (in blocks below the committed tip) kept in the [`FlashblockIndex`]
/// after each canonical commit, before older entries are pruned. Generous
/// enough to absorb reorgs/late notifications while bounding memory.
const PRUNE_MARGIN: u64 = 64;

/// Websocket ping interval for the Flashblocks subscription. Matches the
/// cadence used elsewhere in the node's flashblocks tooling.
const FLASHBLOCKS_PING_INTERVAL: Duration = Duration::from_secs(5);

/// Builds a [`FlashblockIndex`] and, if `MEV_FLASHBLOCKS_URL` is set and
/// non-empty, starts a Flashblocks websocket subscription that populates it.
///
/// Failure-isolated by construction: a missing env var, an unparseable URL, or
/// any subscriber error is logged via `warn!` and the (empty) index is returned
/// so the `ExEx` falls back to the block-hash placeholder. The websocket can
/// NEVER take the `ExEx` (or node) down.
fn start_flashblocks_index() -> FlashblockIndex {
    let index = FlashblockIndex::new();
    let url = match std::env::var("MEV_FLASHBLOCKS_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            info!(
                target: "base::mev_emitter",
                "MEV_FLASHBLOCKS_URL unset; flashblock attribution disabled (placeholder used)",
            );
            return index;
        }
    };
    match url.parse() {
        Ok(ws_url) => {
            let receiver = Arc::new(EmitterFlashblocksReceiver::new(index.clone()));
            let mut subscriber =
                FlashblocksSubscriber::new(receiver, ws_url, FLASHBLOCKS_PING_INTERVAL);
            subscriber.start();
            info!(
                target: "base::mev_emitter",
                url = %url,
                "flashblock attribution enabled",
            );
        }
        Err(e) => warn!(
            target: "base::mev_emitter",
            url = %url,
            error = %e,
            "invalid MEV_FLASHBLOCKS_URL; flashblock attribution disabled (placeholder used)",
        ),
    }
    index
}

/// `ExEx` run loop: drain canonical-chain notifications, report `FinishedHeight`.
pub async fn run_mev_emitter_exex(mut ctx: ExExContext<BaseNodeAdapter>) -> eyre::Result<()> {
    // C-2 ①: the EVM configuration used to re-execute committed transactions (the
    // per-tx `EvmState` source for `revm_bridge`). `chain_spec` comes from the
    // ExEx provider; `BaseEvmConfig::base` wires the mainnet receipt builder.
    let evm_config = BaseEvmConfig::base(ctx.provider().chain_spec());
    let registry = crate::state_diff::BalanceSlotRegistry::base_priority();
    // C-3: real flashblock payloadId/index attribution. The subscription is
    // failure-isolated — on any error the index stays empty and the loop falls
    // back to the block-hash placeholder.
    let index = start_flashblocks_index();
    info!(target: "base::mev_emitter", "mev-emitter ExEx started");
    ctx.notifications.set_without_head();
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(committed) = notification.committed_chain() {
            let (mut total_diffs, mut total_cands, mut total_trusted) = (0usize, 0usize, 0usize);
            // Count txs whose payloadId came from the flashblock index (real
            // attribution) vs the block-hash placeholder.
            let mut total_fb_attributed = 0usize;
            for (&block_number, block) in committed.blocks() {
                // Re-execution is isolated per block: any failure is logged and
                // skipped, NEVER propagated — an ExEx error would otherwise crash
                // the whole node (ExEx is a critical task). The emitter must never
                // be able to take the node down.
                let block_result: eyre::Result<(usize, usize, usize, usize)> = (|| {
                    // C-2 ②: the parent state to re-execute this block's txs against.
                    let parent = block_number.saturating_sub(1);
                    let db = StateProviderDatabase::new(
                        ctx.provider().history_by_block_number(parent)?,
                    );
                    // C-2 ③④: a commit-capable revm State over that DB + the Base
                    // EVM configured for this block's environment.
                    let state =
                        State::builder().with_database(db).with_bundle_update().build();
                    let evm_env = evm_config.evm_env(block.header())?;
                    let mut evm = evm_config.evm_with_env(state, evm_env);
                    // Canonical blocks carry no flashblock payloadId on the header;
                    // the block hash is the stable fallback when the flashblock
                    // index has no entry for a tx (deposit/system txs, or blocks
                    // executed before the subscription was up).
                    let placeholder = format!("{:#x}", block.hash());
                    // C-2 ⑤: re-execute each tx; derive per-tx StateDiffEvents from
                    // its EvmState + Transfer-log candidates, committing between txs.
                    let (mut diffs, mut cands, mut trusted, mut attributed) =
                        (0usize, 0usize, 0usize, 0usize);
                    for tx in block.transactions_recovered() {
                        let out = evm.transact(evm_config.tx_env(tx))?;
                        // Diagnostics: did this tx's EvmState touch a trusted token
                        // contract, and how many Transfer-log candidates did it yield?
                        trusted += out.state.keys().filter(|&a| registry.is_trusted(a)).count();
                        let candidates = crate::candidates::transfer_candidates(
                            out.result.logs().iter().map(|l| l.topics()),
                        );
                        cands += candidates.len();
                        // C-3: real (payload_id, flashblock_index) from the index,
                        // falling back to the block-hash placeholder at index 0.
                        let (payload_id, fb_index) =
                            index.lookup(block_number, tx.tx_hash()).map_or_else(
                                || (placeholder.clone(), 0u32),
                                |found| {
                                    attributed += 1;
                                    found
                                },
                            );
                        let events = crate::revm_bridge::state_diffs_from_evm_state(
                            &out.state,
                            &registry,
                            &candidates,
                            tx.tx_hash(),
                            block_number,
                            fb_index,
                            payload_id,
                        );
                        diffs += events.len();
                        // TODO(C-4): emit `events` over the outbound transport.
                        evm.db_mut().commit(out.state);
                    }
                    Ok((diffs, cands, trusted, attributed))
                })();
                match block_result {
                    Ok((d, c, t, a)) => {
                        total_diffs += d;
                        total_cands += c;
                        total_trusted += t;
                        total_fb_attributed += a;
                    }
                    Err(e) => warn!(
                        target: "base::mev_emitter",
                        block = block_number,
                        error = %e,
                        "block re-execution failed; skipped (node unaffected)",
                    ),
                }
            }
            let tip = committed.tip().num_hash();
            // C-3: bound index memory — drop attribution for blocks well below
            // the tip (margin absorbs reorgs/late notifications).
            index.prune_below(tip.number.saturating_sub(PRUNE_MARGIN));
            debug!(
                target: "base::mev_emitter",
                number = tip.number,
                blocks = committed.blocks().len(),
                state_diffs = total_diffs,
                candidates = total_cands,
                trusted_touched = total_trusted,
                fb_attributed = total_fb_attributed,
                "chain committed",
            );
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
