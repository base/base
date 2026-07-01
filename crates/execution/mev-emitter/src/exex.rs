//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

use alloy_evm::Evm;
use alloy_network_primitives::TransactionResponse;
use alloy_primitives::{Address, B256};
use base_execution_evm::BaseEvmConfig;
use base_flashblocks::{FlashblocksAPI, FlashblocksState, FlashblocksSubscriber, PendingBlocks};
use base_node_runner::{BaseNodeAdapter, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use futures::TryStreamExt;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_exex::{ExExContext, ExExEvent, ExExNotificationsStream};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use revm::DatabaseCommit;
use revm::database::State;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::flashblocks::{EmitterFlashblocksReceiver, FlashblockIndex};
use crate::transport::EventSink;

/// Margin (in blocks below the committed tip) kept in the [`FlashblockIndex`]
/// after each canonical commit, before older entries are pruned. Generous
/// enough to absorb reorgs/late notifications while bounding memory.
const PRUNE_MARGIN: u64 = 64;

/// Websocket ping interval for the Flashblocks subscription. Matches the
/// cadence used elsewhere in the node's flashblocks tooling.
const FLASHBLOCKS_PING_INTERVAL: Duration = Duration::from_secs(5);

/// Explicit opt-in for ahead-of-committed preconf emission. `MEV_EMITTER_ENABLE`
/// still gates the whole ExEx; this narrower switch keeps the committed-chain
/// emitter behavior unchanged unless operators set `MEV_EMITTER_PRECONF=1`.
const PRECONF_ENV: &str = "MEV_EMITTER_PRECONF";
/// Explicit opt-in for in-node arbitrage dry-run observations.
const ARB_DRYRUN_ENV: &str = crate::arb_dryrun::ARB_DRYRUN_ENV;
/// Optional per-frame dirty pool cap for dry-run work.
const ARB_DRYRUN_MAX_POOLS_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_MAX_POOLS";
/// Optional per-frame candidate cap for dry-run work.
const ARB_DRYRUN_MAX_CANDIDATES_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_MAX_CANDIDATES";
/// Optional per-frame wall-clock budget in microseconds.
const ARB_DRYRUN_TIME_BUDGET_MICROS_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_TIME_BUDGET_MICROS";
/// Optional exact-input amount used for dry-run estimates.
const ARB_DRYRUN_AMOUNT_IN_WEI_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_AMOUNT_IN_WEI";

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

/// Returns true only for the explicit `MEV_EMITTER_PRECONF=1` opt-in.
pub fn preconf_emission_enabled() -> bool {
    preconf_emission_enabled_from_value(std::env::var_os(PRECONF_ENV).as_deref())
}

fn preconf_emission_enabled_from_value(value: Option<&OsStr>) -> bool {
    value.and_then(OsStr::to_str).is_some_and(|raw| raw.trim() == "1")
}

/// Returns true only for the explicit `MEV_EMITTER_ARB_DRYRUN=1` opt-in.
pub fn arb_dryrun_enabled() -> bool {
    crate::arb_dryrun::enabled_from_value(std::env::var_os(ARB_DRYRUN_ENV).as_deref())
}

fn arb_dryrun_config_from_env() -> crate::arb_dryrun::DryRunConfig {
    let mut config = crate::arb_dryrun::DryRunConfig::default();
    if let Some(value) = parse_env_usize(ARB_DRYRUN_MAX_POOLS_ENV) {
        config.max_pools_per_frame = value;
    }
    if let Some(value) = parse_env_usize(ARB_DRYRUN_MAX_CANDIDATES_ENV) {
        config.max_candidates_per_frame = value;
    }
    if let Some(value) = parse_env_u64(ARB_DRYRUN_TIME_BUDGET_MICROS_ENV) {
        config.time_budget = Duration::from_micros(value);
    }
    if let Some(value) = parse_env_u128(ARB_DRYRUN_AMOUNT_IN_WEI_ENV) {
        config.amount_in_wei = value;
    }
    config
}

fn parse_env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn parse_env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn parse_env_u128(name: &str) -> Option<u128> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn emit_arb_dryrun_frame(
    sink: &EventSink,
    block_number: u64,
    flashblock_index: u32,
    payload_id: String,
    dirty_pools: BTreeSet<Address>,
    config: &crate::arb_dryrun::DryRunConfig,
) {
    if dirty_pools.is_empty() {
        return;
    }
    let frame = crate::arb_dryrun::run_frame(
        &[],
        &dirty_pools,
        config,
        crate::arb_dryrun::NoActionGuard,
    );
    if frame.candidates.is_empty() {
        sink.send_event(&crate::NodeEvent::ArbDryRunObservation(
            crate::ArbDryRunObservationEvent {
                protocol_version: crate::arb_dryrun::ARB_DRYRUN_PROTOCOL_VERSION,
                block_number,
                flashblock_index,
                payload_id,
                dirty_pool_count: u32::try_from(frame.dirty_pool_count).unwrap_or(u32::MAX),
                candidate_fingerprint: "health".to_string(),
                candidate_key: "health".to_string(),
                tokens: Vec::new(),
                pools: dirty_pools.into_iter().collect(),
                protocols: Vec::new(),
                amount_in_wei: config.amount_in_wei,
                estimated_gross_wei: 0,
                estimated_net_wei: 0,
                approximation: false,
                caveat: Some(
                    frame
                        .caveat
                        .unwrap_or_else(|| "pool-baseline-unavailable-in-rust-phase1".to_string()),
                ),
                latency_micros: frame.latency_micros,
                truncated: frame.truncated,
                health: frame.health,
            },
        ));
        return;
    }
    for candidate in frame.candidates {
        sink.send_event(&crate::NodeEvent::ArbDryRunObservation(
            crate::ArbDryRunObservationEvent {
                protocol_version: crate::arb_dryrun::ARB_DRYRUN_PROTOCOL_VERSION,
                block_number,
                flashblock_index,
                payload_id: payload_id.clone(),
                dirty_pool_count: u32::try_from(frame.dirty_pool_count).unwrap_or(u32::MAX),
                candidate_fingerprint: candidate.fingerprint,
                candidate_key: candidate.candidate_id,
                tokens: candidate.tokens,
                pools: candidate.pools,
                protocols: candidate.protocols.iter().map(|p| p.as_str().to_string()).collect(),
                amount_in_wei: config.amount_in_wei,
                estimated_gross_wei: candidate.estimated_gross_wei,
                estimated_net_wei: candidate.estimated_net_wei,
                approximation: candidate.approximation,
                caveat: candidate.caveat.or_else(|| frame.caveat.clone()),
                latency_micros: frame.latency_micros,
                truncated: frame.truncated,
                health: frame.health.clone(),
            },
        ));
    }
}

/// Issue #45: emit pool storage-slot diffs from one FLASHBLOCK PRECONFIRMATION.
///
/// This is the genuinely AHEAD-OF-COMMITTED price source. The `base-flashblocks`
/// crate reconstructs preconfirmed pending state ~0.2–2s before canonical commit;
/// for the latest flashblock delta we re-use the EXACT same extraction the
/// committed loop uses ([`crate::candidates::swap_pool_candidates`] +
/// [`crate::revm_bridge::pool_slot_diffs_from_evm_state`]) over the per-tx
/// `EvmState` the flashblocks crate already captured. The events carry the
/// flashblock `payload_id`/`index`, so downstream `MidBlockSlotAggregator` keys by
/// `payload_id` and reconciles/discards on preconf reorg.
///
/// Total by construction: no `unwrap`/panic — it runs in a spawned task next to a
/// critical ExEx and must never be able to take the node down.
///
/// v1 limitation: re-emits the latest pending flashblock each time it ticks
/// (idempotent per `payload_id`+pool+slot, so the downstream aggregator dedups).
/// Reserve-pool (Aerodrome / UniV2) slot SEMANTIC decoding remains a downstream TS
/// concern — this path emits raw `(slot, post-value)` words like the committed loop.
fn emit_preconf_pool_slots(
    pb: &PendingBlocks,
    sink: &EventSink,
    arb_dryrun: Option<&crate::arb_dryrun::DryRunConfig>,
) {
    let block_number = pb.latest_block_number();
    let payload_id = format!("{}", pb.payload_id());
    let fb_index = pb.latest_flashblock_index() as u32;
    let mut dirty_pools = BTreeSet::new();
    for twl in pb.get_latest_flashblock_transactions_with_logs() {
        let tx_hash = twl.transaction.tx_hash();
        // Pool addresses (Swap-log emitters) — mirrors the committed loop's
        // `swap_pool_candidates(... map(|l| (l.address, l.topics())))`. Flashblock
        // logs are `alloy_rpc_types_eth::Log`, so `address()`/`topics()` accessors.
        let pool_candidates = crate::candidates::swap_pool_candidates(
            twl.logs.iter().map(|l| (l.address(), l.topics())),
        );
        if pool_candidates.is_empty() {
            continue;
        }
        let Some(evm_state) = pb.get_transaction_state(&tx_hash) else {
            continue;
        };
        let events = crate::revm_bridge::pool_slot_diffs_from_evm_state(
            &evm_state,
            &pool_candidates,
            tx_hash,
            block_number,
            fb_index,
            payload_id.clone(),
        );
        for ev in events {
            dirty_pools.insert(ev.pool);
            sink.send_event(&crate::NodeEvent::PoolSlotDiff(ev));
        }
    }
    if let Some(config) = arb_dryrun {
        emit_arb_dryrun_frame(sink, block_number, fb_index, payload_id, dirty_pools, config);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreconfPayloadRef {
    payload_id: String,
    block_number: u64,
    parent_hash: B256,
}

impl PreconfPayloadRef {
    fn from_pending(pending: &PendingBlocks) -> Self {
        Self {
            payload_id: format!("{}", pending.payload_id()),
            block_number: pending.latest_block_number(),
            parent_hash: pending.parent_hash(),
        }
    }
}

/// Issue #45 (critic HIGH, base#4): decide whether a newly-received preconf
/// payload invalidates the previous one. Same-height payload replacement is a
/// `Superseded` discard; same/lower-height parent changes are `Reorg` discards.
/// Height advances do not discard here because the committed-chain boundary
/// reconciles finalized payloads.
fn preconf_discard(
    last: Option<&PreconfPayloadRef>,
    next: &PreconfPayloadRef,
) -> Option<crate::DiscardPreconfEvent> {
    let prev = last?;
    let reason = if next.block_number < prev.block_number
        || (next.block_number == prev.block_number && next.parent_hash != prev.parent_hash)
    {
        crate::DiscardReason::Reorg
    } else if next.block_number == prev.block_number && next.payload_id != prev.payload_id {
        crate::DiscardReason::Superseded
    } else {
        return None;
    };
    Some(crate::DiscardPreconfEvent {
        protocol_version: crate::PROTOCOL_VERSION,
        payload_id: prev.payload_id.clone(),
        block_number: Some(prev.block_number),
        reason: Some(reason),
    })
}

#[cfg(test)]
mod preconf_tests {
    use super::{PreconfPayloadRef, preconf_discard, preconf_emission_enabled_from_value};
    use crate::DiscardReason;
    use alloy_primitives::B256;
    use std::ffi::OsStr;

    fn payload(payload_id: &str, block_number: u64, parent_byte: u8) -> PreconfPayloadRef {
        PreconfPayloadRef {
            payload_id: payload_id.to_string(),
            block_number,
            parent_hash: B256::from([parent_byte; 32]),
        }
    }

    #[test]
    fn preconf_env_requires_explicit_one() {
        assert!(!preconf_emission_enabled_from_value(None));
        assert!(!preconf_emission_enabled_from_value(Some(OsStr::new(""))));
        assert!(!preconf_emission_enabled_from_value(Some(OsStr::new("true"))));
        assert!(!preconf_emission_enabled_from_value(Some(OsStr::new("0"))));
        assert!(preconf_emission_enabled_from_value(Some(OsStr::new("1"))));
        assert!(preconf_emission_enabled_from_value(Some(OsStr::new(" 1 "))));
    }

    #[test]
    fn emits_discard_for_superseded_and_reorged_preconf_payloads() {
        let a = payload("A", 100, 1);
        assert!(preconf_discard(None, &a).is_none());
        assert!(preconf_discard(Some(&a), &payload("A", 100, 1)).is_none());

        let superseded = preconf_discard(Some(&a), &payload("B", 100, 1))
            .expect("same-height payload change must emit a discard");
        assert_eq!(superseded.payload_id, "A");
        assert_eq!(superseded.block_number, Some(100));
        assert_eq!(superseded.reason, Some(DiscardReason::Superseded));

        let same_height_reorg = preconf_discard(Some(&a), &payload("C", 100, 2))
            .expect("same-height parent change must emit a reorg discard");
        assert_eq!(same_height_reorg.payload_id, "A");
        assert_eq!(same_height_reorg.block_number, Some(100));
        assert_eq!(same_height_reorg.reason, Some(DiscardReason::Reorg));

        let lower_height_reorg = preconf_discard(Some(&a), &payload("D", 99, 1))
            .expect("height rollback must emit a reorg discard");
        assert_eq!(lower_height_reorg.reason, Some(DiscardReason::Reorg));

        assert!(preconf_discard(Some(&a), &payload("C", 101, 2)).is_none());
    }
}

/// `ExEx` run loop: drain canonical-chain notifications, report `FinishedHeight`.
///
/// `fb_state` (issue #45): when present and `MEV_EMITTER_PRECONF=1`, a
/// failure-isolated background task subscribes to flashblock preconfirmations and
/// emits ahead-of-committed pool-slot diffs via [`emit_preconf_pool_slots`]. The
/// committed-chain loop below is UNCHANGED and remains the finalizing/reconciling
/// path (its `BlockBoundaryEvent { finalized: true }` reconciles the preconf
/// stream).
pub async fn run_mev_emitter_exex(
    mut ctx: ExExContext<BaseNodeAdapter>,
    fb_state: Option<Arc<FlashblocksState>>,
) -> eyre::Result<()> {
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
    let arb_dryrun = if arb_dryrun_enabled() {
        let config = arb_dryrun_config_from_env();
        let time_budget_micros =
            u64::try_from(config.time_budget.as_micros()).unwrap_or(u64::MAX);
        info!(
            target: "base::mev_emitter",
            max_pools = config.max_pools_per_frame,
            max_candidates = config.max_candidates_per_frame,
            time_budget_micros,
            amount_in_wei = %config.amount_in_wei,
            "arb dry-run observations enabled (MEV_EMITTER_ARB_DRYRUN=1)",
        );
        Some(config)
    } else {
        info!(
            target: "base::mev_emitter",
            "arb dry-run observations disabled (set MEV_EMITTER_ARB_DRYRUN=1 to enable)",
        );
        None
    };
    // Issue #45: ahead-of-committed preconf pool-slot emission. Failure-isolated in
    // its own task — a lagged/closed receiver only ends the task, never the ExEx.
    // This path is explicitly gated by MEV_EMITTER_PRECONF=1 so enabling the
    // committed-chain emitter alone keeps zero preconf behavior.
    if preconf_emission_enabled() {
        if let Some(state) = fb_state {
            let task_sink = sink.clone();
            let mut rx = state.subscribe_to_flashblocks();
            let task_arb_dryrun = arb_dryrun.clone();
            tokio::spawn(async move {
                // Tracks the previous preconf so reorg/supersede signals can discard
                // stale pending slots before they are committed/finalized.
                let mut last_preconf: Option<PreconfPayloadRef> = None;
                loop {
                    match rx.recv().await {
                        Ok(pending) => {
                            // Issue #45 (critic HIGH, base#4): when a NEW payload
                            // invalidates the previous one, emit discard_preconf so
                            // downstream MidBlockSlotAggregator drops stale slots.
                            let next_preconf = PreconfPayloadRef::from_pending(&pending);
                            if let Some(ev) = preconf_discard(last_preconf.as_ref(), &next_preconf)
                            {
                                task_sink.send_event(&crate::NodeEvent::DiscardPreconf(ev));
                            }
                            emit_preconf_pool_slots(&pending, &task_sink, task_arb_dryrun.as_ref());
                            last_preconf = Some(next_preconf);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                target: "base::mev_emitter",
                                skipped = n,
                                "preconf flashblock receiver lagged",
                            );
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            info!(
                target: "base::mev_emitter",
                "preconf pool-slot emission enabled (MEV_EMITTER_PRECONF=1)",
            );
        } else {
            warn!(
                target: "base::mev_emitter",
                "MEV_EMITTER_PRECONF=1 set but flashblocks state unavailable; preconf emission disabled",
            );
        }
    } else {
        info!(
            target: "base::mev_emitter",
            "preconf pool-slot emission disabled (set MEV_EMITTER_PRECONF=1 to enable)",
        );
    }
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
                    let db =
                        StateProviderDatabase::new(ctx.provider().history_by_block_number(parent)?);
                    // C-2 ③④: a commit-capable revm State over that DB + the Base
                    // EVM configured for this block's environment.
                    let state = State::builder().with_database(db).with_bundle_update().build();
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
                    let mut arb_dirty_by_payload: HashMap<String, (u32, BTreeSet<Address>)> =
                        HashMap::new();
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
                        let native_events =
                            crate::revm_bridge::native_balance_diffs_from_evm_state(
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
                        if arb_dryrun.is_some() && !pool_slot_events.is_empty() {
                            let entry = arb_dirty_by_payload
                                .entry(payload_id.clone())
                                .or_insert_with(|| (fb_index, BTreeSet::new()));
                            entry.0 = entry.0.max(fb_index);
                            entry.1.extend(pool_slot_events.iter().map(|ev| ev.pool));
                        }
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
                    if let Some(config) = arb_dryrun.as_ref() {
                        for (payload_id, (fb_index, dirty_pools)) in arb_dirty_by_payload {
                            emit_arb_dryrun_frame(
                                &sink,
                                block_number,
                                fb_index,
                                payload_id,
                                dirty_pools,
                                config,
                            );
                        }
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
                })(
                );
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
///
/// `fb_state` (issue #45): the shared [`FlashblocksState`] (cloned from the
/// flashblocks extension's config) the ExEx subscribes to for ahead-of-committed
/// preconf pool-slot emission only when `MEV_EMITTER_PRECONF=1`. `None` or an
/// unset preconf env keeps committed-loop behavior only.
#[derive(Debug)]
pub struct MevEmitterExtension {
    fb_state: Option<Arc<FlashblocksState>>,
}

impl FromExtensionConfig for MevEmitterExtension {
    type Config = Option<Arc<FlashblocksState>>;

    fn from_config(config: Self::Config) -> Self {
        Self { fb_state: config }
    }
}

impl BaseNodeExtension for MevEmitterExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let fb_state = self.fb_state;
        hooks.install_exex("mev-emitter", move |ctx| {
            let fb_state = fb_state.clone();
            async move { Ok(run_mev_emitter_exex(ctx, fb_state)) }
        })
    }
}
