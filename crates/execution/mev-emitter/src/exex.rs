//! C-1: `MevEmitter` ExEx — the non-invasive node hook.
//!
//! Installs a reth Execution Extension that observes `ChainCommitted`
//! notifications on the canonical chain. For C-1 it is a skeleton: it logs
//! committed tips and reports `FinishedHeight` (so the node can prune
//! ExEx-held data), establishing the wiring that the later increments build on:
//! C-2 attaches a revm `Inspector` here to capture per-tx token state-diffs,
//! C-3 folds in Flashblocks, and C-4 streams the encoded events to the TS
//! `ProviderNodeStream` consumer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_evm::Evm;
use alloy_network_primitives::TransactionResponse;
use alloy_primitives::{Address, B256, U256};
use base_execution_evm::BaseEvmConfig;
use base_flashblocks::{
    FlashblocksAPI, FlashblocksState, FlashblocksSubscriber, PendingBlocks, PendingFrameObserver,
};
use base_node_runner::{BaseNodeAdapter, BaseNodeExtension, FromExtensionConfig, NodeHooks};
use futures::TryStreamExt;
use reth_chainspec::ChainSpecProvider;
use reth_evm::ConfigureEvm;
use reth_exex::{ExExContext, ExExEvent, ExExNotificationsStream};
use reth_provider::StateProviderFactory;
use reth_revm::database::StateProviderDatabase;
use revm::database::State;
use revm::{Database, DatabaseCommit};
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
/// Optional maximum cycle length, counted in graph legs.
const ARB_DRYRUN_MAX_CYCLE_LEGS_ENV: &str = "MEV_EMITTER_MAX_CYCLE_LEGS";
/// Off-by-default provider refresh for dry-run candidate member reserves.
const ARB_DRYRUN_LIVE_RESERVE_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_LIVE_RESERVE";
/// Optional per-frame dirty pool cap for dry-run work.
const ARB_DRYRUN_MAX_POOLS_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_MAX_POOLS";
/// Optional per-frame candidate cap for dry-run work.
const ARB_DRYRUN_MAX_CANDIDATES_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_MAX_CANDIDATES";
/// Optional per-frame wall-clock budget in microseconds.
const ARB_DRYRUN_TIME_BUDGET_MICROS_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_TIME_BUDGET_MICROS";
/// Optional exact-input amount used for dry-run estimates.
const ARB_DRYRUN_AMOUNT_IN_WEI_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_AMOUNT_IN_WEI";
/// Optional decoded pool baseline JSON file for runtime candidate evaluation.
const ARB_DRYRUN_POOL_BASELINE_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_POOL_BASELINE";
/// Optional L2 gas cost for dry-run signed net metadata.
const ARB_DRYRUN_L2_GAS_COST_WEI_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_L2_GAS_COST_WEI";
/// Optional L1 base fee for dry-run signed net metadata.
const ARB_DRYRUN_L1_BASE_FEE_WEI_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_L1_BASE_FEE_WEI";
/// Optional blob base fee for dry-run signed net metadata.
const ARB_DRYRUN_BLOB_BASE_FEE_WEI_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_BLOB_BASE_FEE_WEI";
/// Optional Ecotone base fee scalar for dry-run signed net metadata.
const ARB_DRYRUN_BASE_FEE_SCALAR_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_BASE_FEE_SCALAR";
/// Optional Ecotone blob base fee scalar for dry-run signed net metadata.
const ARB_DRYRUN_BLOB_BASE_FEE_SCALAR_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_BLOB_BASE_FEE_SCALAR";
/// Optional calldata hex for dry-run signed net metadata.
const ARB_DRYRUN_CALLDATA_HEX_ENV: &str = "MEV_EMITTER_ARB_DRYRUN_CALLDATA_HEX";

#[derive(Debug, Clone)]
struct ArbDryRunRuntime {
    config: crate::arb_dryrun::DryRunConfig,
    pools: Arc<Vec<crate::arb_dryrun::PoolState>>,
    pool_index: Arc<HashMap<Address, usize>>,
    live_reserve: bool,
}

#[derive(Debug, Clone)]
struct CoreArbPendingFrameObserver {
    sink: EventSink,
    arb_dryrun: ArbDryRunRuntime,
}

impl CoreArbPendingFrameObserver {
    fn new(sink: EventSink, arb_dryrun: ArbDryRunRuntime) -> Self {
        Self { sink, arb_dryrun }
    }
}

impl PendingFrameObserver for CoreArbPendingFrameObserver {
    fn on_pending_frame(&self, pending: &PendingBlocks) {
        emit_preconf_pool_slots(pending, &self.sink, Some(&self.arb_dryrun), false);
    }
}

#[derive(Debug, Clone)]
struct ArbDirtyState {
    flashblock_index: u32,
    dirty_pools: BTreeSet<Address>,
    deltas: BTreeMap<Address, crate::arb_dryrun::PoolStateDelta>,
    raw_slots: BTreeMap<Address, BTreeMap<U256, U256>>,
    fallback_elapsed: Duration,
    fallback_attempted: usize,
    fallback_pools: BTreeSet<Address>,
}

impl ArbDirtyState {
    const fn new(flashblock_index: u32) -> Self {
        Self {
            flashblock_index,
            dirty_pools: BTreeSet::new(),
            deltas: BTreeMap::new(),
            raw_slots: BTreeMap::new(),
            fallback_elapsed: Duration::ZERO,
            fallback_attempted: 0,
            fallback_pools: BTreeSet::new(),
        }
    }
}
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
/// Returns true only when dry-run is explicitly enabled and live reserve is exactly `1`.
pub fn arb_dryrun_live_reserve_enabled() -> bool {
    arb_dryrun_live_reserve_enabled_from_values(
        std::env::var_os(ARB_DRYRUN_ENV).as_deref(),
        std::env::var_os(ARB_DRYRUN_LIVE_RESERVE_ENV).as_deref(),
    )
}

fn arb_dryrun_live_reserve_enabled_from_values(
    dryrun: Option<&OsStr>,
    live_reserve: Option<&OsStr>,
) -> bool {
    crate::arb_dryrun::enabled_from_value(dryrun)
        && crate::arb_dryrun::enabled_from_value(live_reserve)
}

fn arb_dryrun_config_from_env() -> crate::arb_dryrun::DryRunConfig {
    let mut config = crate::arb_dryrun::DryRunConfig::default();
    config.max_cycle_legs = arb_dryrun_max_cycle_legs_from_value(
        std::env::var_os(ARB_DRYRUN_MAX_CYCLE_LEGS_ENV).as_deref(),
    );
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
    config.net_cost = arb_dryrun_net_cost_config_from_env();
    config.clamped()
}

fn arb_dryrun_max_cycle_legs_from_value(value: Option<&OsStr>) -> usize {
    let Some(max_cycle_legs) =
        value.and_then(OsStr::to_str).and_then(|raw| raw.trim().parse::<usize>().ok())
    else {
        return crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT;
    };

    crate::arb_dryrun::DryRunConfig { max_cycle_legs, ..crate::arb_dryrun::DryRunConfig::default() }
        .clamped()
        .max_cycle_legs
}

fn arb_dryrun_net_cost_config_from_env() -> Option<crate::arb_dryrun::DryRunNetCostConfig> {
    Some(crate::arb_dryrun::DryRunNetCostConfig {
        l2_gas_cost_wei: parse_env_u128(ARB_DRYRUN_L2_GAS_COST_WEI_ENV)?,
        l1_data_fee: crate::arb_dryrun::EcotoneL1DataFeeConfig {
            calldata: parse_env_calldata(ARB_DRYRUN_CALLDATA_HEX_ENV)?,
            l1_base_fee_wei: parse_env_u128(ARB_DRYRUN_L1_BASE_FEE_WEI_ENV)?,
            blob_base_fee_wei: parse_env_u128(ARB_DRYRUN_BLOB_BASE_FEE_WEI_ENV)?,
            base_fee_scalar: parse_env_u128(ARB_DRYRUN_BASE_FEE_SCALAR_ENV)?,
            blob_base_fee_scalar: parse_env_u128(ARB_DRYRUN_BLOB_BASE_FEE_SCALAR_ENV)?,
        },
    })
}

fn arb_dryrun_runtime_from_env() -> Option<ArbDryRunRuntime> {
    let config = arb_dryrun_config_from_env();
    let baseline_path = std::env::var(ARB_DRYRUN_POOL_BASELINE_ENV).ok();
    let live_reserve = arb_dryrun_live_reserve_enabled();
    arb_dryrun_runtime_from_baseline_path(config, baseline_path.as_deref(), live_reserve)
}

fn build_arb_dryrun_runtime(
    config: crate::arb_dryrun::DryRunConfig,
    pools: Vec<crate::arb_dryrun::PoolState>,
    live_reserve: bool,
) -> ArbDryRunRuntime {
    let pool_index = pools.iter().enumerate().fold(HashMap::new(), |mut index, (i, pool)| {
        index.entry(pool.pool).or_insert(i);
        index
    });
    ArbDryRunRuntime {
        config,
        pools: Arc::new(pools),
        pool_index: Arc::new(pool_index),
        live_reserve,
    }
}

fn arb_dryrun_runtime_from_baseline_path(
    config: crate::arb_dryrun::DryRunConfig,
    baseline_path: Option<&str>,
    live_reserve: bool,
) -> Option<ArbDryRunRuntime> {
    let Some(path) = baseline_path.map(str::trim).filter(|path| !path.is_empty()) else {
        info!(
            target: "base::mev_emitter",
            "arb dry-run observations disabled: MEV_EMITTER_ARB_DRYRUN_POOL_BASELINE is unset",
        );
        return None;
    };
    let pools = match crate::arb_dryrun::load_pool_baseline_from_path(path) {
        Ok(pools) if !pools.is_empty() => pools,
        Ok(_) => {
            warn!(
                target: "base::mev_emitter",
                path = %path,
                "arb dry-run observations disabled: pool baseline is empty",
            );
            return None;
        }
        Err(err) => {
            warn!(
                target: "base::mev_emitter",
                path = %path,
                error = %err,
                "arb dry-run observations disabled: pool baseline unavailable",
            );
            return None;
        }
    };
    Some(build_arb_dryrun_runtime(config, pools, live_reserve))
}

#[cfg(test)]
fn core_arb_dryrun_runtime_from_baseline_path(
    config: crate::arb_dryrun::DryRunConfig,
    baseline_path: Option<&str>,
) -> Option<ArbDryRunRuntime> {
    arb_dryrun_runtime_from_baseline_path(config, baseline_path, false)
}

fn preconf_subscriber_arb_dryrun(
    arb_dryrun_frame_source_installed: bool,
    arb_dryrun: Option<ArbDryRunRuntime>,
) -> Option<ArbDryRunRuntime> {
    if arb_dryrun_frame_source_installed { None } else { arb_dryrun }
}

/// WP-S1a placement decision: provider/MDBX reserve refresh must not run inside
/// `PendingFrameObserver::on_pending_frame`. That observer is invoked synchronously by
/// `base-flashblocks` while its StateProcessor is inside `block_in_place`; the
/// `drop(live_state)` patch releases the pending-state mutex before observer notification, but
/// MDBX reads there would still extend the flashblock processor's critical section. The live-reserve
/// path therefore replaces `CoreArbPendingFrameObserver` with this independent broadcast subscriber,
/// which runs after processed pending frames are published and is gated only by dry-run +
/// live-reserve flags, not `MEV_EMITTER_PRECONF`. While live-reserve is on, dry-run observation is
/// post-publication, not the synchronous ahead-of-committed observer path.
fn start_arb_dryrun_live_reserve_worker<Provider>(
    fb_state: &Arc<FlashblocksState>,
    sink: EventSink,
    runtime: ArbDryRunRuntime,
    provider: Provider,
) where
    Provider: StateProviderFactory + Clone + Send + 'static,
{
    let mut rx = fb_state.subscribe_to_flashblocks();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(pending) => {
                    let parent = pending.earliest_block_number().saturating_sub(1);
                    match provider.history_by_block_number(parent) {
                        Ok(state_provider) => {
                            let mut db = StateProviderDatabase::new(state_provider);
                            emit_preconf_pool_slots_with_live_reserve(
                                &pending, &sink, &runtime, &mut db,
                            );
                        }
                        Err(error) => {
                            warn!(
                                target: "base::mev_emitter",
                                block = parent,
                                error = %error,
                                "arb dry-run live-reserve provider unavailable",
                            );
                            emit_preconf_pool_slots_with_live_reserve_error(
                                &pending, &sink, &runtime,
                            );
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        target: "base::mev_emitter",
                        skipped = n,
                        "arb dry-run live-reserve receiver lagged",
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
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
fn parse_env_calldata(name: &str) -> Option<Vec<u8>> {
    parse_hex_bytes(std::env::var(name).ok()?.trim())
}

fn parse_hex_bytes(raw: &str) -> Option<Vec<u8>> {
    let hex = raw.strip_prefix("0x").unwrap_or(raw);
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks_exact(2) {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn baseline_pool<'a>(
    runtime: &'a ArbDryRunRuntime,
    pool: &Address,
) -> Option<&'a crate::arb_dryrun::PoolState> {
    runtime.pool_index.get(pool).and_then(|&i| runtime.pools.get(i))
}

fn selected_dirty_pools_for_fallback(
    runtime: &ArbDryRunRuntime,
    dirty_state: &ArbDirtyState,
) -> Vec<Address> {
    let mut selected_indexes = BTreeSet::new();
    for pool in &dirty_state.dirty_pools {
        if let Some(index) = runtime.pool_index.get(pool) {
            selected_indexes.insert(*index);
        }
    }
    selected_indexes
        .into_iter()
        .take(runtime.config.max_pools_per_frame)
        .filter_map(|index| runtime.pools.get(index).map(|pool| pool.pool))
        .collect()
}

fn record_pool_slot_event(
    runtime: &ArbDryRunRuntime,
    dirty_state: &mut ArbDirtyState,
    event: &crate::PoolSlotDiffEvent,
    block_number: u64,
) {
    dirty_state.flashblock_index = dirty_state.flashblock_index.max(event.flashblock_index);
    dirty_state.dirty_pools.insert(event.pool);
    let slots = dirty_state.raw_slots.entry(event.pool).or_default();
    slots.insert(event.slot, event.value);
    let Some(pool) = baseline_pool(runtime, &event.pool) else {
        return;
    };
    let mut delta = crate::arb_dryrun::PoolStateDelta::from_slots_with_live_reserve(
        pool,
        slots,
        crate::arb_dryrun::PoolOverlaySource::SlotDiff,
        Some(block_number),
        Some(block_number),
        runtime.live_reserve,
    );
    if let Some(existing) = dirty_state.deltas.get(&event.pool) {
        if existing.is_live_read_complete() && !delta.is_live_read_complete() {
            let mut preserved = existing.clone();
            for caveat in &delta.caveats {
                preserved.add_caveat(caveat);
            }
            dirty_state.deltas.insert(event.pool, preserved);
            return;
        }
        for caveat in &existing.caveats {
            delta.add_caveat(caveat);
        }
    }
    dirty_state.deltas.insert(event.pool, delta);
}

fn mark_fallback_caveat(
    dirty_state: &mut ArbDirtyState,
    pool: Address,
    block_number: u64,
    caveat: &str,
) {
    let delta = dirty_state.deltas.entry(pool).or_insert_with(|| {
        crate::arb_dryrun::PoolStateDelta::new(
            crate::arb_dryrun::PoolOverlaySource::StateProvider,
            Some(block_number),
        )
    });
    delta.add_caveat(caveat);
}

fn merge_fallback_delta(
    dirty_state: &mut ArbDirtyState,
    pool: Address,
    delta: crate::arb_dryrun::PoolStateDelta,
) {
    if let Some(existing) = dirty_state.deltas.get_mut(&pool) {
        for caveat in &delta.caveats {
            existing.add_caveat(caveat);
        }
        if existing.is_live_read_complete() {
            return;
        }
        if existing.source == crate::arb_dryrun::PoolOverlaySource::SlotDiff {
            merge_missing_fallback_fields(existing, &delta);
            return;
        }
    }
    if delta.has_state_update() || delta.is_live_read_complete() {
        dirty_state.deltas.insert(pool, delta);
        return;
    }
    let entry = dirty_state.deltas.entry(pool).or_insert_with(|| delta.clone());
    for caveat in &delta.caveats {
        entry.add_caveat(caveat);
    }
}

fn merge_missing_fallback_fields(
    existing: &mut crate::arb_dryrun::PoolStateDelta,
    fallback: &crate::arb_dryrun::PoolStateDelta,
) {
    if existing.reserve0.is_none() {
        existing.reserve0 = fallback.reserve0;
    }
    if existing.reserve1.is_none() {
        existing.reserve1 = fallback.reserve1;
    }
    if existing.sqrt_price_x96.is_none() {
        existing.sqrt_price_x96 = fallback.sqrt_price_x96;
    }
    if existing.liquidity.is_none() {
        existing.liquidity = fallback.liquidity;
    }
    if existing.tick.is_none() {
        existing.tick = fallback.tick;
    }
    existing.live_read_complete |= fallback.is_live_read_complete();
    if existing.live_read_complete {
        existing.caveats.retain(|caveat| caveat != "partial-live-overlay");
    }
}

fn refresh_fallback_pools<DB, I>(
    db: &mut DB,
    runtime: &ArbDryRunRuntime,
    dirty_state: &mut ArbDirtyState,
    pools_to_refresh: I,
    block_number: u64,
    fallback_cap: usize,
) where
    DB: Database,
    I: IntoIterator<Item = Address>,
{
    let started = Instant::now();
    for pool_addr in pools_to_refresh {
        if dirty_state.fallback_pools.contains(&pool_addr) {
            continue;
        }
        if dirty_state
            .deltas
            .get(&pool_addr)
            .is_some_and(crate::arb_dryrun::PoolStateDelta::is_live_read_complete)
        {
            continue;
        }
        if dirty_state.fallback_attempted >= fallback_cap {
            mark_fallback_caveat(dirty_state, pool_addr, block_number, "fallback-cap-exhausted");
            dirty_state.fallback_pools.insert(pool_addr);
            continue;
        }
        if dirty_state.fallback_elapsed.saturating_add(started.elapsed())
            >= runtime.config.time_budget
        {
            mark_fallback_caveat(dirty_state, pool_addr, block_number, "fallback-timeout");
            dirty_state.fallback_pools.insert(pool_addr);
            continue;
        }
        let Some(pool) = baseline_pool(runtime, &pool_addr) else {
            continue;
        };
        let slots_to_read =
            crate::arb_dryrun::fallback_overlay_slots_with_live_reserve(pool, runtime.live_reserve);
        if slots_to_read.is_empty() {
            mark_fallback_caveat(
                dirty_state,
                pool_addr,
                block_number,
                "fallback-unsupported-protocol",
            );
            dirty_state.fallback_pools.insert(pool_addr);
            continue;
        }
        let mut slots = BTreeMap::new();
        let mut failed = false;
        dirty_state.fallback_attempted += 1;
        dirty_state.fallback_pools.insert(pool_addr);
        for slot in slots_to_read {
            if dirty_state.fallback_elapsed.saturating_add(started.elapsed())
                >= runtime.config.time_budget
            {
                mark_fallback_caveat(dirty_state, pool_addr, block_number, "fallback-timeout");
                failed = true;
                break;
            }
            let key = crate::arb_dryrun::slot_key(*slot);
            match db.storage(pool_addr, key) {
                Ok(value) => {
                    slots.insert(key, value);
                }
                Err(error) => {
                    warn!(
                        target: "base::mev_emitter",
                        pool = %pool_addr,
                        slot = *slot,
                        error = ?error,
                        "arb dry-run fallback storage read failed",
                    );
                    mark_fallback_caveat(
                        dirty_state,
                        pool_addr,
                        block_number,
                        "fallback-provider-error",
                    );
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        let raw_slots = dirty_state.raw_slots.entry(pool_addr).or_default();
        for (slot, value) in &slots {
            raw_slots.insert(*slot, *value);
        }
        let mut delta = crate::arb_dryrun::PoolStateDelta::from_slots_with_live_reserve(
            pool,
            &slots,
            crate::arb_dryrun::PoolOverlaySource::StateProvider,
            Some(block_number),
            Some(block_number),
            runtime.live_reserve,
        );
        if !delta.has_state_update() && delta.caveats.is_empty() {
            delta.add_caveat("live-overlay-verified");
        }
        merge_fallback_delta(dirty_state, pool_addr, delta);
    }
    dirty_state.fallback_elapsed = dirty_state.fallback_elapsed.saturating_add(started.elapsed());
}

fn supplement_committed_fallback<DB: Database>(
    db: &mut DB,
    runtime: &ArbDryRunRuntime,
    dirty_state: &mut ArbDirtyState,
    block_number: u64,
) {
    let fallback_cap = runtime.config.max_pools_per_frame.min(8);
    let dirty_pools = selected_dirty_pools_for_fallback(runtime, dirty_state);
    refresh_fallback_pools(db, runtime, dirty_state, dirty_pools, block_number, fallback_cap);
}

fn arb_dryrun_observation_events(
    block_number: u64,
    flashblock_index: u32,
    payload_id: &str,
    amount_in_wei: u128,
    frame: crate::arb_dryrun::DryRunFrame,
) -> Vec<crate::NodeEvent> {
    if frame.candidates.is_empty() {
        return vec![crate::NodeEvent::ArbDryRunObservation(crate::ArbDryRunObservationEvent {
            protocol_version: crate::arb_dryrun::ARB_DRYRUN_PROTOCOL_VERSION,
            block_number,
            flashblock_index,
            payload_id: payload_id.to_string(),
            dirty_pool_count: u32::try_from(frame.dirty_pool_count).unwrap_or(u32::MAX),
            candidate_fingerprint: "health".to_string(),
            candidate_key: "health".to_string(),
            tokens: Vec::new(),
            pools: Vec::new(),
            protocols: Vec::new(),
            amount_in_wei,
            estimated_gross_wei: 0,
            estimated_net_wei: None,
            approximation: false,
            caveat: Some(
                frame
                    .caveat
                    .unwrap_or_else(|| "pool-baseline-unavailable-in-rust-phase1".to_string()),
            ),
            latency_micros: frame.latency_micros,
            truncated: frame.truncated,
            health: frame.health,
        })];
    }

    let truncated_caveat = frame.truncated.then_some("partial-frame-truncated");
    let frame_caveat = frame.caveat;
    let dirty_pool_count = u32::try_from(frame.dirty_pool_count).unwrap_or(u32::MAX);
    let latency_micros = frame.latency_micros;
    let truncated = frame.truncated;
    let health = frame.health;
    frame
        .candidates
        .into_iter()
        .map(|candidate| {
            let protocols = candidate.protocols.iter().map(|p| p.as_str().to_string()).collect();
            let caveat = crate::arb_dryrun::compose_caveat_parts(&[
                frame_caveat.as_deref(),
                candidate.caveat.as_deref(),
                truncated_caveat,
            ]);
            crate::NodeEvent::ArbDryRunObservation(crate::ArbDryRunObservationEvent {
                protocol_version: crate::arb_dryrun::ARB_DRYRUN_PROTOCOL_VERSION,
                block_number,
                flashblock_index,
                payload_id: payload_id.to_string(),
                dirty_pool_count,
                candidate_fingerprint: candidate.fingerprint,
                candidate_key: candidate.candidate_id,
                tokens: candidate.tokens,
                pools: candidate.pools,
                protocols,
                amount_in_wei,
                estimated_gross_wei: candidate.estimated_gross_wei,
                estimated_net_wei: candidate.estimated_net_wei,
                approximation: candidate.approximation,
                caveat,
                latency_micros,
                truncated,
                health: health.clone(),
            })
        })
        .collect()
}

fn build_arb_dryrun_frame(
    runtime: &ArbDryRunRuntime,
    dirty_state: &ArbDirtyState,
) -> crate::arb_dryrun::DryRunFrame {
    let mut frame_config = runtime.config.clone();
    frame_config.time_budget =
        frame_config.time_budget.saturating_sub(dirty_state.fallback_elapsed);
    crate::arb_dryrun::run_frame_with_overlay(
        runtime.pools.as_slice(),
        &dirty_state.dirty_pools,
        &dirty_state.deltas,
        &frame_config,
        crate::arb_dryrun::NoActionGuard,
    )
}

fn candidate_member_pools(frame: &crate::arb_dryrun::DryRunFrame) -> BTreeSet<Address> {
    frame.candidates.iter().flat_map(|candidate| candidate.pools.iter().copied()).collect()
}

fn send_arb_dryrun_frame(
    sink: &EventSink,
    block_number: u64,
    payload_id: &str,
    flashblock_index: u32,
    runtime: &ArbDryRunRuntime,
    frame: crate::arb_dryrun::DryRunFrame,
) {
    for event in arb_dryrun_observation_events(
        block_number,
        flashblock_index,
        payload_id,
        runtime.config.amount_in_wei,
        frame,
    ) {
        sink.send_event(&event);
    }
}

fn emit_arb_dryrun_frame(
    sink: &EventSink,
    block_number: u64,
    payload_id: String,
    dirty_state: ArbDirtyState,
    runtime: &ArbDryRunRuntime,
) {
    if dirty_state.dirty_pools.is_empty() {
        return;
    }
    let frame = build_arb_dryrun_frame(runtime, &dirty_state);
    send_arb_dryrun_frame(
        sink,
        block_number,
        &payload_id,
        dirty_state.flashblock_index,
        runtime,
        frame,
    );
}

fn emit_arb_dryrun_frame_with_refresh<DB: Database>(
    sink: &EventSink,
    block_number: u64,
    payload_id: String,
    mut dirty_state: ArbDirtyState,
    runtime: &ArbDryRunRuntime,
    db: &mut DB,
) {
    if dirty_state.dirty_pools.is_empty() {
        return;
    }
    let pass1 = build_arb_dryrun_frame(runtime, &dirty_state);
    let member_pools = candidate_member_pools(&pass1);
    if member_pools.is_empty() {
        send_arb_dryrun_frame(
            sink,
            block_number,
            &payload_id,
            dirty_state.flashblock_index,
            runtime,
            pass1,
        );
        return;
    }
    refresh_fallback_pools(
        db,
        runtime,
        &mut dirty_state,
        member_pools,
        block_number,
        runtime.config.max_pools_per_frame,
    );
    let pass2 = build_arb_dryrun_frame(runtime, &dirty_state);
    send_arb_dryrun_frame(
        sink,
        block_number,
        &payload_id,
        dirty_state.flashblock_index,
        runtime,
        pass2,
    );
}

fn emit_arb_dryrun_frame_with_refresh_error(
    sink: &EventSink,
    block_number: u64,
    payload_id: String,
    mut dirty_state: ArbDirtyState,
    runtime: &ArbDryRunRuntime,
) {
    if dirty_state.dirty_pools.is_empty() {
        return;
    }
    let pass1 = build_arb_dryrun_frame(runtime, &dirty_state);
    let member_pools = candidate_member_pools(&pass1);
    if member_pools.is_empty() {
        send_arb_dryrun_frame(
            sink,
            block_number,
            &payload_id,
            dirty_state.flashblock_index,
            runtime,
            pass1,
        );
        return;
    }
    for pool in member_pools {
        mark_fallback_caveat(&mut dirty_state, pool, block_number, "fallback-provider-error");
    }
    let pass2 = build_arb_dryrun_frame(runtime, &dirty_state);
    send_arb_dryrun_frame(
        sink,
        block_number,
        &payload_id,
        dirty_state.flashblock_index,
        runtime,
        pass2,
    );
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
/// The raw `(slot, post-value)` stream remains unchanged; the optional arb
/// dry-run lane also decodes those words into an in-node live-state overlay.
fn collect_preconf_dirty_state(
    pb: &PendingBlocks,
    sink: &EventSink,
    arb_dryrun: Option<&ArbDryRunRuntime>,
    emit_pool_slot_diffs: bool,
) -> (u64, String, ArbDirtyState) {
    let block_number = pb.latest_block_number();
    let payload_id = format!("{}", pb.payload_id());
    let fb_index = pb.latest_flashblock_index() as u32;
    let mut dirty_state = ArbDirtyState::new(fb_index);
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
            if let Some(runtime) = arb_dryrun {
                record_pool_slot_event(runtime, &mut dirty_state, &ev, block_number);
            }
            if emit_pool_slot_diffs {
                sink.send_event(&crate::NodeEvent::PoolSlotDiff(ev));
            }
        }
    }
    (block_number, payload_id, dirty_state)
}

fn emit_preconf_pool_slots(
    pb: &PendingBlocks,
    sink: &EventSink,
    arb_dryrun: Option<&ArbDryRunRuntime>,
    emit_pool_slot_diffs: bool,
) {
    let (block_number, payload_id, dirty_state) =
        collect_preconf_dirty_state(pb, sink, arb_dryrun, emit_pool_slot_diffs);
    if let Some(runtime) = arb_dryrun {
        emit_arb_dryrun_frame(sink, block_number, payload_id, dirty_state, runtime);
    }
}

fn emit_preconf_pool_slots_with_live_reserve<DB: Database>(
    pb: &PendingBlocks,
    sink: &EventSink,
    runtime: &ArbDryRunRuntime,
    db: &mut DB,
) {
    let (block_number, payload_id, dirty_state) =
        collect_preconf_dirty_state(pb, sink, Some(runtime), false);
    emit_arb_dryrun_frame_with_refresh(sink, block_number, payload_id, dirty_state, runtime, db);
}

fn emit_preconf_pool_slots_with_live_reserve_error(
    pb: &PendingBlocks,
    sink: &EventSink,
    runtime: &ArbDryRunRuntime,
) {
    let (block_number, payload_id, dirty_state) =
        collect_preconf_dirty_state(pb, sink, Some(runtime), false);
    emit_arb_dryrun_frame_with_refresh_error(sink, block_number, payload_id, dirty_state, runtime);
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
    use super::{
        PreconfPayloadRef, arb_dryrun_live_reserve_enabled_from_values, preconf_discard,
        preconf_emission_enabled_from_value,
    };
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
    fn live_reserve_env_requires_dryrun_and_explicit_one() {
        assert!(!arb_dryrun_live_reserve_enabled_from_values(None, Some(OsStr::new("1"))));
        assert!(!arb_dryrun_live_reserve_enabled_from_values(Some(OsStr::new("1")), None,));
        assert!(!arb_dryrun_live_reserve_enabled_from_values(
            Some(OsStr::new("1")),
            Some(OsStr::new("true")),
        ));
        assert!(arb_dryrun_live_reserve_enabled_from_values(
            Some(OsStr::new("1")),
            Some(OsStr::new("1")),
        ));
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
/// stream). When `MEV_EMITTER_ARB_DRYRUN_LIVE_RESERVE=1`, the arb dry-run path
/// intentionally uses a post-publication broadcast worker instead of the synchronous
/// `CoreArbPendingFrameObserver`, so ahead-of-committed dry-run observations disappear.
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
    let sink = crate::transport::shared_event_sink();
    let arb_dryrun = if arb_dryrun_enabled() {
        let runtime = arb_dryrun_runtime_from_env();
        if let Some(runtime) = &runtime {
            let time_budget_micros =
                u64::try_from(runtime.config.time_budget.as_micros()).unwrap_or(u64::MAX);
            info!(
                target: "base::mev_emitter",
                max_pools = runtime.config.max_pools_per_frame,
                max_candidates = runtime.config.max_candidates_per_frame,
                time_budget_micros,
                amount_in_wei = %runtime.config.amount_in_wei,
                baseline_pools = runtime.pools.len(),
                live_reserve = runtime.live_reserve,
                "arb dry-run observations enabled (MEV_EMITTER_ARB_DRYRUN=1)",
            );
        }
        runtime
    } else {
        info!(
            target: "base::mev_emitter",
            "arb dry-run observations disabled (set MEV_EMITTER_ARB_DRYRUN=1 to enable)",
        );
        None
    };
    let arb_dryrun_frame_source_installed = fb_state.is_some() && arb_dryrun.is_some();
    if let (Some(state), Some(runtime)) = (fb_state.as_ref(), arb_dryrun.clone()) {
        if runtime.live_reserve {
            // Live-reserve provider reads deliberately replace the synchronous
            // `CoreArbPendingFrameObserver`; this avoids MDBX work in the flashblocks
            // processor and means dry-run observations are no longer ahead-of-committed.
            start_arb_dryrun_live_reserve_worker(
                state,
                sink.clone(),
                runtime,
                ctx.provider().clone(),
            );
            info!(
                target: "base::mev_emitter",
                "arb dry-run live-reserve worker installed on flashblocks broadcast stream",
            );
        } else {
            state.set_pending_frame_observer(Some(Arc::new(CoreArbPendingFrameObserver::new(
                sink.clone(),
                runtime,
            ))));
            info!(
                target: "base::mev_emitter",
                "core arb dry-run hook installed on flashblocks StateProcessor",
            );
        }
    }
    // Issue #45: ahead-of-committed preconf pool-slot emission. Failure-isolated in
    // its own task — a lagged/closed receiver only ends the task, never the ExEx.
    // This path is explicitly gated by MEV_EMITTER_PRECONF=1 so enabling the
    // committed-chain emitter alone keeps zero preconf behavior.
    if preconf_emission_enabled() {
        if let Some(state) = fb_state {
            let task_sink = sink.clone();
            let mut rx = state.subscribe_to_flashblocks();
            let task_arb_dryrun = preconf_subscriber_arb_dryrun(
                arb_dryrun_frame_source_installed,
                arb_dryrun.clone(),
            );
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
                            emit_preconf_pool_slots(
                                &pending,
                                &task_sink,
                                task_arb_dryrun.as_ref(),
                                true,
                            );
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
                    let mut arb_dirty_by_payload: HashMap<String, ArbDirtyState> = HashMap::new();
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
                        let payload_for_arb = payload_id.clone();
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
                        if let Some(runtime) = arb_dryrun.as_ref()
                            && !pool_slot_events.is_empty()
                        {
                            let entry = arb_dirty_by_payload
                                .entry(payload_for_arb.clone())
                                .or_insert_with(|| ArbDirtyState::new(fb_index));
                            entry.flashblock_index = entry.flashblock_index.max(fb_index);
                            for event in &pool_slot_events {
                                record_pool_slot_event(runtime, entry, event, block_number);
                            }
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
                        for ev in &pool_slot_events {
                            sink.send_event(&crate::NodeEvent::PoolSlotDiff(ev.clone()));
                            sent_this_block += 1;
                        }
                        // COMMIT (precondition anchor): advances db to POST-tx state.
                        // Native baseline read above relies on this happening AFTER.
                        evm.db_mut().commit(out.state);
                        if let Some(runtime) = arb_dryrun.as_ref()
                            && let Some(entry) = arb_dirty_by_payload.get_mut(&payload_for_arb)
                        {
                            supplement_committed_fallback(
                                evm.db_mut(),
                                runtime,
                                entry,
                                block_number,
                            );
                        }
                    }
                    if let Some(runtime) = arb_dryrun.as_ref() {
                        for (payload_id, dirty_state) in arb_dirty_by_payload {
                            if runtime.live_reserve {
                                emit_arb_dryrun_frame_with_refresh(
                                    &sink,
                                    block_number,
                                    payload_id,
                                    dirty_state,
                                    runtime,
                                    evm.db_mut(),
                                );
                            } else {
                                emit_arb_dryrun_frame(
                                    &sink,
                                    block_number,
                                    payload_id,
                                    dirty_state,
                                    runtime,
                                );
                            }
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

#[cfg(test)]
mod arb_overlay_tests {
    use super::*;
    use crate::PoolSlotDiffEvent;
    use crate::arb_dryrun::{Protocol, slot_key};
    use revm::state::{AccountInfo, Bytecode};
    #[cfg(unix)]
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    use std::{fs, path::PathBuf};

    const MAX_CYCLE_LEGS_CHILD_ENV: &str = "BASE_MEV_EMITTER_MAX_CYCLE_LEGS_CHILD";
    const MAX_CYCLE_LEGS_CHILD_TEST: &str =
        "exex::arb_overlay_tests::max_cycle_legs_env_wiring_is_process_isolated";

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn runtime_with_pools(pools: Vec<crate::arb_dryrun::PoolState>) -> ArbDryRunRuntime {
        let pool_index = pools.iter().enumerate().fold(HashMap::new(), |mut index, (i, pool)| {
            index.insert(pool.pool, i);
            index
        });
        ArbDryRunRuntime {
            config: crate::arb_dryrun::DryRunConfig::default(),
            pools: Arc::new(pools),
            pool_index: Arc::new(pool_index),
            live_reserve: false,
        }
    }

    fn runtime_with_pool(pool: crate::arb_dryrun::PoolState) -> ArbDryRunRuntime {
        runtime_with_pools(vec![pool])
    }

    fn temp_baseline_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("base-mev-core-arb-hook-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn max_cycle_legs_value_parser_is_total_and_clamped() {
        let cases = [
            (None, crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some(""), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some(" \t\n"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("two"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("-1"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("184467440737095516160"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("0"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("1"), crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT),
            (Some("2"), 2),
            (Some("+3"), 3),
            (Some(" \t+3\n"), 3),
            (Some("3"), 3),
            (Some("4"), crate::arb_dryrun::HARD_MAX_CYCLE_LEGS),
            (Some("5"), crate::arb_dryrun::HARD_MAX_CYCLE_LEGS),
        ];

        for (raw, expected) in cases {
            assert_eq!(
                arb_dryrun_max_cycle_legs_from_value(raw.map(OsStr::new)),
                expected,
                "unexpected max cycle legs for {raw:?}"
            );
        }

        #[cfg(unix)]
        {
            let non_unicode = OsString::from_vec(vec![0xff]);
            assert_eq!(
                arb_dryrun_max_cycle_legs_from_value(Some(non_unicode.as_os_str())),
                crate::arb_dryrun::MAX_CYCLE_LEGS_DEFAULT
            );
        }
    }

    #[test]
    fn max_cycle_legs_env_wiring_is_process_isolated() {
        if std::env::var_os(MAX_CYCLE_LEGS_CHILD_ENV).is_some() {
            assert_eq!(arb_dryrun_config_from_env().max_cycle_legs, 3);
            return;
        }

        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", MAX_CYCLE_LEGS_CHILD_TEST, "--nocapture"])
            .env(MAX_CYCLE_LEGS_CHILD_ENV, "1")
            .env(ARB_DRYRUN_MAX_CYCLE_LEGS_ENV, " \t+3\n")
            .status()
            .expect("run isolated max-cycle-legs wiring child");
        assert!(status.success(), "isolated max-cycle-legs wiring child failed");
    }

    #[test]
    fn core_arb_runtime_requires_non_empty_baseline() {
        let config = crate::arb_dryrun::DryRunConfig::default();
        assert!(core_arb_dryrun_runtime_from_baseline_path(config.clone(), None).is_none());
        assert!(core_arb_dryrun_runtime_from_baseline_path(config.clone(), Some(" ")).is_none());
        assert!(
            core_arb_dryrun_runtime_from_baseline_path(
                config.clone(),
                Some("/tmp/base-mev-missing-core-arb-baseline.json"),
            )
            .is_none()
        );

        let empty_path = temp_baseline_path("empty");
        fs::write(&empty_path, "[]").expect("write empty baseline");
        let empty_path_string = empty_path.display().to_string();
        assert!(
            core_arb_dryrun_runtime_from_baseline_path(config.clone(), Some(&empty_path_string),)
                .is_none()
        );
        let _ = fs::remove_file(&empty_path);

        let pool = v2_pool(0xc1, 1_000, 2_000);
        let non_empty_path = temp_baseline_path("non-empty");
        fs::write(
            &non_empty_path,
            serde_json::to_string(&vec![pool.clone()]).expect("serialize baseline"),
        )
        .expect("write non-empty baseline");
        let non_empty_path_string = non_empty_path.display().to_string();
        let runtime =
            core_arb_dryrun_runtime_from_baseline_path(config, Some(&non_empty_path_string))
                .expect("non-empty baseline should enable core runtime");
        let _ = fs::remove_file(&non_empty_path);

        assert_eq!(runtime.pools.len(), 1);
        assert_eq!(runtime.pools[0], pool);
    }
    #[test]
    fn preconf_subscriber_arb_runtime_is_suppressed_when_frame_source_installed() {
        let runtime = runtime_with_pool(v2_pool(0xc2, 1_000, 2_000));

        assert!(preconf_subscriber_arb_dryrun(false, Some(runtime.clone())).is_some());
        assert!(preconf_subscriber_arb_dryrun(true, Some(runtime)).is_none());
        let mut live_runtime = runtime_with_pool(v2_pool(0xc3, 1_000, 2_000));
        live_runtime.live_reserve = true;
        assert!(preconf_subscriber_arb_dryrun(true, Some(live_runtime)).is_none());
        assert!(preconf_subscriber_arb_dryrun(false, None).is_none());
    }

    fn pack_univ2_reserves(reserve0: u128, reserve1: u128) -> U256 {
        U256::from(reserve0) | (U256::from(reserve1) << 112usize)
    }

    fn pack_slot0_word(sqrt_price_x96: u128, tick: i32) -> U256 {
        let tick_u24 = if tick < 0 {
            u32::try_from((1i64 << 24) + i64::from(tick)).unwrap()
        } else {
            u32::try_from(tick).unwrap()
        };
        U256::from(sqrt_price_x96) | (U256::from(tick_u24) << 160usize)
    }

    #[derive(Debug)]
    struct StorageDbError;

    impl std::fmt::Display for StorageDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("storage db error")
        }
    }

    impl std::error::Error for StorageDbError {}

    impl revm::context::DBErrorMarker for StorageDbError {}

    #[derive(Default)]
    struct StorageDb {
        slots: BTreeMap<(Address, U256), U256>,
        fail_slots: BTreeSet<(Address, U256)>,
        storage_calls: usize,
    }

    impl StorageDb {
        fn with_slot(mut self, pool: Address, slot: U256, value: U256) -> Self {
            self.slots.insert((pool, slot), value);
            self
        }

        fn with_failed_slot(mut self, pool: Address, slot: U256) -> Self {
            self.fail_slots.insert((pool, slot));
            self
        }
    }

    impl Database for StorageDb {
        type Error = StorageDbError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            self.storage_calls += 1;
            if self.fail_slots.contains(&(address, index)) {
                return Err(StorageDbError);
            }
            Ok(*self.slots.get(&(address, index)).unwrap_or(&U256::ZERO))
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn pool_slot_event_records_semantic_overlay_delta() {
        let pool = crate::arb_dryrun::PoolState::v2_like(
            addr(0xa1),
            Protocol::UniswapV2,
            addr(1),
            addr(2),
            30,
            1_000,
            2_000,
        );
        let runtime = runtime_with_pool(pool.clone());
        let mut dirty_state = ArbDirtyState::new(0);
        let event = PoolSlotDiffEvent {
            protocol_version: crate::PROTOCOL_VERSION,
            tx_hash: B256::from([0x33; 32]),
            block_number: 42,
            flashblock_index: 3,
            payload_id: "payload".to_string(),
            pool: pool.pool,
            slot: slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT),
            value: pack_univ2_reserves(3_000, 4_000),
        };

        record_pool_slot_event(&runtime, &mut dirty_state, &event, 42);

        assert!(dirty_state.dirty_pools.contains(&pool.pool));
        assert_eq!(dirty_state.flashblock_index, 3);
        let delta = dirty_state.deltas.get(&pool.pool).expect("semantic delta");
        assert_eq!(delta.reserve0, Some(3_000));
        assert_eq!(delta.reserve1, Some(4_000));
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));
    }

    fn v2_pool(byte: u8, reserve0: u128, reserve1: u128) -> crate::arb_dryrun::PoolState {
        crate::arb_dryrun::PoolState::v2_like(
            addr(byte),
            Protocol::UniswapV2,
            addr(1),
            addr(2),
            30,
            reserve0,
            reserve1,
        )
    }

    fn dirty_state_for(pool: Address) -> ArbDirtyState {
        let mut dirty_state = ArbDirtyState::new(0);
        dirty_state.dirty_pools.insert(pool);
        dirty_state
    }

    #[test]
    fn committed_fallback_updates_dirty_delta_from_storage() {
        let pool = v2_pool(0xa2, 1_000, 2_000);
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db =
            StorageDb::default().with_slot(pool.pool, slot, pack_univ2_reserves(3_000, 4_000));

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 1);
        let delta = dirty_state.deltas.get(&pool.pool).expect("fallback delta");
        assert_eq!(delta.reserve0, Some(3_000));
        assert_eq!(delta.reserve1, Some(4_000));
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));
    }

    #[test]
    fn committed_fallback_marks_equal_live_read_verified_not_stale() {
        let pool = v2_pool(0xa3, 1_000, 2_000);
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db =
            StorageDb::default().with_slot(pool.pool, slot, pack_univ2_reserves(1_000, 2_000));

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);
        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 1);
        let delta = dirty_state.deltas.get(&pool.pool).expect("fallback delta");
        assert!(!delta.has_state_update());
        assert!(delta.is_live_read_complete());
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-verified"));
        assert!(!delta.caveats.iter().any(|c| c == "stale-baseline-fallback"));
    }

    #[test]
    fn committed_fallback_provider_error_adds_caveat_without_panic() {
        let pool = v2_pool(0xa4, 1_000, 2_000);
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db = StorageDb::default().with_failed_slot(pool.pool, slot);

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);
        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 1);
        let delta = dirty_state.deltas.get(&pool.pool).expect("fallback caveat");
        assert!(delta.caveats.iter().any(|c| c == "fallback-provider-error"));
    }

    #[test]
    fn committed_fallback_cap_is_cumulative_per_payload() {
        let pools = (0u8..9).map(|i| v2_pool(0xb0 + i, 1_000, 2_000)).collect::<Vec<_>>();
        let mut runtime = runtime_with_pools(pools.clone());
        runtime.config.max_pools_per_frame = 9;
        runtime.config.time_budget = Duration::from_secs(1);
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = ArbDirtyState::new(0);
        let mut db = StorageDb::default();
        for (i, pool) in pools.iter().enumerate() {
            dirty_state.dirty_pools.insert(pool.pool);
            db = db.with_slot(
                pool.pool,
                slot,
                pack_univ2_reserves(3_000 + i as u128, 4_000 + i as u128),
            );
        }

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);
        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 8);
        assert_eq!(db.storage_calls, 8);
        assert!(
            dirty_state
                .deltas
                .get(&pools[0].pool)
                .is_some_and(crate::arb_dryrun::PoolStateDelta::has_state_update)
        );
        let capped = dirty_state.deltas.get(&pools[8].pool).expect("cap caveat");
        assert!(capped.caveats.iter().any(|c| c == "fallback-cap-exhausted"));
    }

    #[test]
    fn committed_fallback_uses_frame_selected_baseline_order_not_address_order() {
        let first_in_baseline = v2_pool(0xf0, 1_000, 2_000);
        let first_by_address = v2_pool(0x01, 1_000, 2_000);
        let mut runtime =
            runtime_with_pools(vec![first_in_baseline.clone(), first_by_address.clone()]);
        runtime.config.max_pools_per_frame = 1;
        runtime.config.time_budget = Duration::from_secs(1);
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = ArbDirtyState::new(0);
        dirty_state.dirty_pools.insert(first_in_baseline.pool);
        dirty_state.dirty_pools.insert(first_by_address.pool);
        let mut db = StorageDb::default()
            .with_slot(first_in_baseline.pool, slot, pack_univ2_reserves(3_000, 4_000))
            .with_slot(first_by_address.pool, slot, pack_univ2_reserves(5_000, 6_000));

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 1);
        assert!(
            dirty_state
                .deltas
                .get(&first_in_baseline.pool)
                .is_some_and(crate::arb_dryrun::PoolStateDelta::has_state_update)
        );
        assert!(!dirty_state.deltas.contains_key(&first_by_address.pool));
    }

    #[test]
    fn committed_fallback_completes_partial_v3_slotdiff() {
        let pool = crate::arb_dryrun::PoolState::v3(
            addr(0xa9),
            addr(1),
            addr(2),
            500,
            100,
            10,
            1,
            Vec::new(),
        );
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let mut dirty_state = ArbDirtyState::new(0);
        let slot0_key = slot_key(crate::arb_dryrun::V3_SLOT0_SLOT);
        let liquidity_key = slot_key(crate::arb_dryrun::V3_LIQUIDITY_SLOT);
        let slot0 = pack_slot0_word(200, 2);
        let event = PoolSlotDiffEvent {
            protocol_version: crate::PROTOCOL_VERSION,
            tx_hash: B256::from([0x44; 32]),
            block_number: 42,
            flashblock_index: 3,
            payload_id: "payload".to_string(),
            pool: pool.pool,
            slot: slot0_key,
            value: slot0,
        };
        record_pool_slot_event(&runtime, &mut dirty_state, &event, 42);
        let partial = dirty_state.deltas.get(&pool.pool).expect("partial delta");
        assert!(!partial.is_live_read_complete());
        assert!(partial.caveats.iter().any(|c| c == "partial-live-overlay"));

        let mut db = StorageDb::default().with_slot(pool.pool, slot0_key, slot0).with_slot(
            pool.pool,
            liquidity_key,
            U256::from(20u128),
        );

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 2);
        let delta = dirty_state.deltas.get(&pool.pool).expect("complete fallback delta");
        assert!(delta.is_live_read_complete());
        assert_eq!(delta.sqrt_price_x96, Some(200));
        assert_eq!(delta.tick, Some(2));
        assert_eq!(delta.liquidity, Some(20));
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));

        let next_slot0 = pack_slot0_word(300, 3);
        let next_event = PoolSlotDiffEvent {
            protocol_version: crate::PROTOCOL_VERSION,
            tx_hash: B256::from([0x55; 32]),
            block_number: 42,
            flashblock_index: 4,
            payload_id: "payload".to_string(),
            pool: pool.pool,
            slot: slot0_key,
            value: next_slot0,
        };
        record_pool_slot_event(&runtime, &mut dirty_state, &next_event, 42);
        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 2);
        let delta = dirty_state.deltas.get(&pool.pool).expect("preserved complete delta");
        assert!(delta.is_live_read_complete());
        assert_eq!(delta.sqrt_price_x96, Some(300));
        assert_eq!(delta.tick, Some(3));
        assert_eq!(delta.liquidity, Some(20));
        assert!(!delta.caveats.iter().any(|c| c == "partial-live-overlay"));
    }

    #[test]
    fn committed_fallback_timeout_prevents_storage_reads() {
        let pool = v2_pool(0xa7, 1_000, 2_000);
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::ZERO;
        let slot = slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT);
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db =
            StorageDb::default().with_slot(pool.pool, slot, pack_univ2_reserves(3_000, 4_000));

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 0);
        assert_eq!(db.storage_calls, 0);
        let delta = dirty_state.deltas.get(&pool.pool).expect("timeout caveat");
        assert!(delta.caveats.iter().any(|c| c == "fallback-timeout"));
    }

    #[test]
    fn committed_fallback_leaves_aerodrome_stable_unsupported_when_live_reserve_off() {
        let pool = crate::arb_dryrun::PoolState::v2_like(
            addr(0xa8),
            Protocol::AerodromeStable,
            addr(1),
            addr(2),
            5,
            1_000,
            2_000,
        );
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db = StorageDb::default()
            .with_slot(
                pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE0_SLOT),
                U256::from(3_000u128),
            )
            .with_slot(
                pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE1_SLOT),
                U256::from(4_000u128),
            );

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 0);
        assert_eq!(db.storage_calls, 0);
        let delta = dirty_state.deltas.get(&pool.pool).expect("unsupported fallback delta");
        assert!(delta.caveats.iter().any(|c| c == "fallback-unsupported-protocol"));
    }

    #[test]
    fn committed_fallback_decodes_aerodrome_stable_reserve_slots_when_live_reserve_on() {
        let pool = crate::arb_dryrun::PoolState::v2_like(
            addr(0xa8),
            Protocol::AerodromeStable,
            addr(1),
            addr(2),
            5,
            1_000,
            2_000,
        );
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        runtime.live_reserve = true;
        let mut dirty_state = dirty_state_for(pool.pool);
        let mut db = StorageDb::default()
            .with_slot(
                pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE0_SLOT),
                U256::from(3_000u128),
            )
            .with_slot(
                pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE1_SLOT),
                U256::from(4_000u128),
            );

        supplement_committed_fallback(&mut db, &runtime, &mut dirty_state, 42);

        assert_eq!(dirty_state.fallback_attempted, 1);
        assert_eq!(db.storage_calls, 2);
        let delta = dirty_state.deltas.get(&pool.pool).expect("stable fallback delta");
        assert_eq!(delta.reserve0, Some(3_000));
        assert_eq!(delta.reserve1, Some(4_000));
        assert!(delta.is_live_read_complete());
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));
    }

    #[test]
    fn candidate_member_refresh_requotes_untouched_aerodrome_stable_pool() {
        let dirty_pool = crate::arb_dryrun::PoolState::v2_like(
            addr(0xd1),
            Protocol::UniswapV2,
            addr(1),
            addr(2),
            1,
            1_000,
            1_100,
        );
        let stable_pool = crate::arb_dryrun::PoolState::v2_like(
            addr(0xd2),
            Protocol::AerodromeStable,
            addr(1),
            addr(2),
            1,
            1_100,
            1_000,
        );
        let mut runtime = runtime_with_pools(vec![dirty_pool.clone(), stable_pool.clone()]);
        runtime.config.time_budget = Duration::from_secs(1);
        runtime.config.max_pools_per_frame = 8;
        runtime.live_reserve = true;
        let dirty_slots = BTreeMap::from([(
            slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT),
            pack_univ2_reserves(1_000, 1_100),
        )]);
        let dirty_delta = crate::arb_dryrun::PoolStateDelta::from_slots(
            &dirty_pool,
            &dirty_slots,
            crate::arb_dryrun::PoolOverlaySource::SlotDiff,
            Some(42),
            Some(42),
        );
        let mut dirty_state = dirty_state_for(dirty_pool.pool);
        dirty_state.deltas.insert(dirty_pool.pool, dirty_delta);
        let pass1 = build_arb_dryrun_frame(&runtime, &dirty_state);
        assert!(pass1.candidates.iter().any(|candidate| {
            candidate.pools.contains(&dirty_pool.pool)
                && candidate.pools.contains(&stable_pool.pool)
        }));
        let pass1_gross = pass1.candidates.first().map(|candidate| candidate.estimated_gross_wei);
        let member_pools = candidate_member_pools(&pass1);
        let mut db = StorageDb::default()
            .with_slot(
                stable_pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE0_SLOT),
                U256::from(1_000u128),
            )
            .with_slot(
                stable_pool.pool,
                slot_key(crate::arb_dryrun::AERO_STABLE_RESERVE1_SLOT),
                U256::from(1_000u128),
            );

        refresh_fallback_pools(
            &mut db,
            &runtime,
            &mut dirty_state,
            member_pools,
            42,
            runtime.config.max_pools_per_frame,
        );

        assert_eq!(dirty_state.fallback_attempted, 1);
        let stable_delta = dirty_state.deltas.get(&stable_pool.pool).expect("stable refreshed");
        assert_eq!(stable_delta.reserve0, Some(1_000));
        assert_eq!(stable_delta.reserve1, Some(1_000));
        assert!(stable_delta.is_live_read_complete());
        let pass2 = build_arb_dryrun_frame(&runtime, &dirty_state);
        let pass2_gross = pass2.candidates.first().map(|candidate| candidate.estimated_gross_wei);
        assert_ne!(pass1_gross, pass2_gross);
    }

    #[test]
    fn candidate_member_refresh_preserves_complete_touched_overlay() {
        let pool = v2_pool(0xd3, 1_000, 2_000);
        let mut runtime = runtime_with_pool(pool.clone());
        runtime.config.time_budget = Duration::from_secs(1);
        let slots = BTreeMap::from([(
            slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT),
            pack_univ2_reserves(3_000, 4_000),
        )]);
        let delta = crate::arb_dryrun::PoolStateDelta::from_slots(
            &pool,
            &slots,
            crate::arb_dryrun::PoolOverlaySource::SlotDiff,
            Some(42),
            Some(42),
        );
        let mut dirty_state = dirty_state_for(pool.pool);
        dirty_state.deltas.insert(pool.pool, delta);
        let mut db = StorageDb::default().with_slot(
            pool.pool,
            slot_key(crate::arb_dryrun::UNIV2_RESERVES_SLOT),
            pack_univ2_reserves(9_000, 9_000),
        );

        refresh_fallback_pools(
            &mut db,
            &runtime,
            &mut dirty_state,
            BTreeSet::from([pool.pool]),
            42,
            runtime.config.max_pools_per_frame,
        );

        assert_eq!(dirty_state.fallback_attempted, 0);
        assert_eq!(db.storage_calls, 0);
        let preserved = dirty_state.deltas.get(&pool.pool).expect("touched overlay");
        assert_eq!(preserved.reserve0, Some(3_000));
        assert_eq!(preserved.reserve1, Some(4_000));
    }
    #[test]
    fn truncated_non_empty_dryrun_frame_emits_candidate_observation() {
        let candidate = crate::arb_dryrun::CycleCandidate {
            tokens: vec![addr(1), addr(2)],
            pools: vec![addr(0xa1), addr(0xa2)],
            protocols: vec![Protocol::UniswapV2, Protocol::UniswapV2],
            fingerprint: "f".repeat(64),
            candidate_id: "candidate".to_string(),
            estimated_gross_wei: 123,
            estimated_net_wei: None,
            approximation: false,
            caveat: Some("candidate-caveat".to_string()),
        };
        let frame = crate::arb_dryrun::DryRunFrame {
            candidates: vec![candidate],
            dirty_pool_count: 1,
            truncated: true,
            health: "truncated".to_string(),
            latency_micros: 7,
            caveat: Some("bounded-frame-truncated".to_string()),
        };

        let events = arb_dryrun_observation_events(42, 3, "payload", 10, frame);

        assert_eq!(events.len(), 1);
        let crate::NodeEvent::ArbDryRunObservation(event) = &events[0] else {
            panic!("expected arb dry-run observation");
        };
        assert_eq!(event.candidate_key, "candidate");
        assert_eq!(event.health, "truncated");
        assert!(event.truncated);
        assert_eq!(event.estimated_gross_wei, 123);
        assert!(event.caveat.as_deref().is_some_and(|caveat| {
            caveat.contains("bounded-frame-truncated")
                && caveat.contains("candidate-caveat")
                && caveat.contains("partial-frame-truncated")
        }));
    }

    #[test]
    fn truncated_empty_dryrun_frame_emits_health_observation() {
        let frame = crate::arb_dryrun::DryRunFrame {
            candidates: Vec::new(),
            dirty_pool_count: 1,
            truncated: true,
            health: "truncated".to_string(),
            latency_micros: 7,
            caveat: Some("bounded-frame-truncated".to_string()),
        };

        let events = arb_dryrun_observation_events(42, 3, "payload", 10, frame);

        assert_eq!(events.len(), 1);
        let crate::NodeEvent::ArbDryRunObservation(event) = &events[0] else {
            panic!("expected arb dry-run observation");
        };
        assert_eq!(event.candidate_key, "health");
        assert_eq!(event.health, "truncated");
        assert!(event.truncated);
        assert_eq!(event.caveat.as_deref(), Some("bounded-frame-truncated"));
    }
}
