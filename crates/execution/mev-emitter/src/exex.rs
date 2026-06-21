//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_evm::Evm;
use alloy_primitives::B256;
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
    // C-4: outbound WebSocket transport. Failure-isolated: a bind failure leaves
    // the sink valid (events go nowhere) and never affects the ExEx. `info!` is
    // emitted inside once the listener is up.
    let sink = crate::transport::start_event_server();
    info!(target: "base::mev_emitter", "mev-emitter ExEx started");
    ctx.notifications.set_without_head();
    while let Some(notification) = ctx.notifications.try_next().await? {
        if let Some(committed) = notification.committed_chain() {
            let (mut total_diffs, mut total_cands, mut total_trusted) = (0usize, 0usize, 0usize);
            // Count txs whose payloadId came from the flashblock index (real
            // attribution) vs the block-hash placeholder.
            let mut total_fb_attributed = 0usize;
            // Count events ACTUALLY streamed out (incremented at the send site),
            // so it stays accurate even when a block fails re-execution partway
            // and its diff counts are discarded.
            let mut total_events_sent = 0usize;
            // C-5: count of (payloadId) boundary pairs emitted this chain, so the
            // TS NodeStreamProcessor can finalize+flush the buffered diffs.
            let mut total_boundaries = 0usize;
            for (&block_number, block) in committed.blocks() {
                // Events written to the wire for THIS block — tracked outside the
                // closure's return so a mid-block failure can't lose the count.
                let mut sent_this_block = 0usize;
                // Re-execution is isolated per block: any failure is logged and
                // skipped, NEVER propagated — an ExEx error would otherwise crash
                // the whole node (ExEx is a critical task). The emitter must never
                // be able to take the node down.
                let block_result: eyre::Result<(usize, usize, usize, usize, usize)> = (|| {
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
                    // C-5: accumulate, per distinct payloadId (insertion-ordered for
                    // determinism — mirrors `state_diff.rs`), every tx hash seen and
                    // the max flashblock_index, so we can emit one synthetic
                    // flashblock frame + boundary per payload after the tx loop. The
                    // `Vec` preserves first-seen order; `fb_order` maps payloadId to
                    // its slot for O(1) accumulation.
                    let mut flashblocks: Vec<(String, (Vec<B256>, u32))> = Vec::new();
                    let mut fb_order: HashMap<String, usize> = HashMap::new();
                    for tx in block.transactions_recovered() {
                        let out = evm.transact(evm_config.tx_env(tx))?;
                        // Diagnostics: did this tx's EvmState touch a trusted token
                        // contract, and how many Transfer-log candidates did it yield?
                        trusted += out.state.keys().filter(|&a| registry.is_trusted(a)).count();
                        let candidates = crate::candidates::transfer_candidates(
                            out.result.logs().iter().map(|l| l.topics()),
                        );
                        cands += candidates.len();
                        // Pool-slot path: the POOL addresses that swapped this tx
                        // (Swap-log emitters). Their changed storage slots carry the
                        // mid-block PRICE state (slot0/liquidity/reserves) the
                        // balance-delta path cannot represent.
                        let pool_candidates = crate::candidates::swap_pool_candidates(
                            out.result.logs().iter().map(|l| (l.address, l.topics())),
                        );
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
                        // C-5: record THIS tx under its payloadId (every tx, not
                        // just those with diffs) and track the max flashblock_index
                        // seen — insertion-ordered, so the post-loop emission is
                        // deterministic. Clone the key before `payload_id` is moved
                        // into `state_diffs_from_evm_state` below.
                        let tx_hash = tx.tx_hash();
                        match fb_order.get(&payload_id) {
                            Some(&i) => {
                                let entry = &mut flashblocks[i].1;
                                entry.0.push(tx_hash);
                                entry.1 = entry.1.max(fb_index);
                            }
                            None => {
                                fb_order.insert(payload_id.clone(), flashblocks.len());
                                flashblocks.push((payload_id.clone(), (vec![tx_hash], fb_index)));
                            }
                        }
                        // WS-E E1: RAW native-ETH deltas for the FULL touched set
                        // (coinbase bribes are native-only with no Transfer log).
                        // COMMIT-ORDERING PRECONDITION: this MUST run BEFORE the
                        // `evm.db_mut().commit(out.state)` below — `db.basic(addr)`
                        // reads the prior-commit (pre-tx) balance as the baseline.
                        // Reordering/batching the commit would corrupt the baseline
                        // and TRIP `revm_bridge::tests` (the pre-tx baseline test).
                        let native_events = crate::revm_bridge::native_balance_diffs_from_evm_state(
                            &out.state,
                            evm.db_mut(),
                            tx_hash,
                            block_number,
                            fb_index,
                            payload_id.clone(),
                        )?;
                        // Pool-slot events: RAW changed storage slots of the pools
                        // that swapped. Computed BEFORE `payload_id` is moved into
                        // `state_diffs_from_evm_state` below (mirrors native's clone).
                        let pool_slot_events = crate::revm_bridge::pool_slot_diffs_from_evm_state(
                            &out.state,
                            &pool_candidates,
                            tx_hash,
                            block_number,
                            fb_index,
                            payload_id.clone(),
                        );
                        let events = crate::revm_bridge::state_diffs_from_evm_state(
                            &out.state,
                            &registry,
                            &candidates,
                            tx_hash,
                            block_number,
                            fb_index,
                            payload_id,
                        );
                        diffs += events.len() + native_events.len();
                        // C-4: stream each per-tx event out over the WebSocket
                        // transport — one `Message::Text` per `encode_event`
                        // string. `send_event` never blocks/panics, so this is
                        // safe inside the critical ExEx task. ERC-20 then native
                        // sentinel rows (both ride the v1 StateDiffEvent shape).
                        for ev in events.iter().chain(native_events.iter()) {
                            sink.send_event(&crate::NodeEvent::StateDiff(ev.clone()));
                            sent_this_block += 1;
                        }
                        // Stream the mid-block pool-slot price signals (distinct
                        // NodeEvent variant). Counted in events_sent via
                        // sent_this_block (telemetry below).
                        for ev in pool_slot_events.iter() {
                            sink.send_event(&crate::NodeEvent::PoolSlotDiff(ev.clone()));
                            sent_this_block += 1;
                        }
                        // COMMIT (precondition anchor): advances db to POST-tx state.
                        // Native baseline read above relies on this happening AFTER.
                        evm.db_mut().commit(out.state);
                    }
                    // C-5: the tx loop completed successfully — emit, per payloadId
                    // in insertion order, a synthetic flashblock frame followed by
                    // its block boundary, AFTER all state-diffs already streamed in
                    // the loop. The TS NodeStreamProcessor needs a frame for a
                    // payloadId before `finalize(payloadId, ...)` returns non-null,
                    // so flashblock MUST precede boundary; the boundary then
                    // finalizes+flushes the buffered diffs. This runs only on a
                    // fully-successful block (a mid-block failure exits the closure
                    // via `?` before reaching here), so a partially re-executed
                    // block is NEVER finalized.
                    let header = block.header();
                    let timestamp = header.timestamp;
                    let state_root = header.state_root;
                    let canonical = block.hash();
                    let boundaries = flashblocks.len();
                    for (payload_id, (tx_hashes, max_index)) in flashblocks {
                        sink.send_event(&crate::NodeEvent::Flashblock(crate::FlashblockEvent {
                            protocol_version: crate::PROTOCOL_VERSION,
                            payload_id: payload_id.clone(),
                            block_number,
                            flashblock_index: max_index,
                            parent_block_hash: None,
                            timestamp,
                            state_root,
                            tx_hashes,
                            finalized: false,
                        }));
                        sent_this_block += 1;
                        sink.send_event(&crate::NodeEvent::BlockBoundary(
                            crate::BlockBoundaryEvent {
                                protocol_version: crate::PROTOCOL_VERSION,
                                payload_id,
                                block_number,
                                canonical_hash: canonical,
                                flashblock_count: 1,
                                finalized: true,
                            },
                        ));
                        sent_this_block += 1;
                    }
                    Ok((diffs, cands, trusted, attributed, boundaries))
                })();
                // Accumulate AFTER the closure regardless of Ok/Err: events sent
                // before a mid-block failure still went out on the wire.
                total_events_sent += sent_this_block;
                match block_result {
                    Ok((d, c, t, a, b)) => {
                        total_diffs += d;
                        total_cands += c;
                        total_trusted += t;
                        total_fb_attributed += a;
                        total_boundaries += b;
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
                // C-4: events actually streamed out (one Message::Text each).
                // Equals state_diffs on fully-successful chains; can exceed it
                // if a block failed re-execution after sending some events.
                events_sent = total_events_sent,
                candidates = total_cands,
                trusted_touched = total_trusted,
                fb_attributed = total_fb_attributed,
                // C-5: flashblock+boundary pairs emitted (one per distinct payloadId
                // across fully-successful blocks), enabling TS finalize+flush.
                boundaries = total_boundaries,
                "chain committed",
            );
            // Report progress so the node can prune ExEx-held data. Don't `?`:
            // a send error only happens when the ExEx manager receiver is gone
            // (node shutting down), and we must never propagate an error out of
            // this critical task on the hot path. Log and continue.
            if let Err(e) = ctx.events.send(ExExEvent::FinishedHeight(tip)) {
                warn!(
                    target: "base::mev_emitter",
                    error = %e,
                    "FinishedHeight send failed (manager gone; node likely stopping)",
                );
            }
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
