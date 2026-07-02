//! Off-by-default, measurement-only arbitrage dry-run helpers.
//!
//! The module is intentionally pure: it performs quote/graph/cycle math and
//! produces observation payloads, but contains no path that can create or send a
//! transaction. Runtime wiring gates every call behind `MEV_EMITTER_ARB_DRYRUN=1`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256, keccak256};
use num_bigint::{BigInt, BigUint};
use num_traits::{One, ToPrimitive, Zero};
use serde::{Deserialize, Serialize};

/// Environment switch for the in-node dry-run lane.
pub const ARB_DRYRUN_ENV: &str = "MEV_EMITTER_ARB_DRYRUN";
/// Current additive dry-run observation protocol.
pub const ARB_DRYRUN_PROTOCOL_VERSION: u32 = 1;
/// Default maximum pools evaluated per frame.
pub const DEFAULT_MAX_POOLS_PER_FRAME: usize = 64;
/// Default maximum candidate loops emitted per frame.
pub const DEFAULT_MAX_CANDIDATES_PER_FRAME: usize = 8;
/// Default wall-clock budget per frame.
pub const DEFAULT_TIME_BUDGET_MICROS: u64 = 2_000;
/// Hard ceiling for env-provided pool caps.
pub const HARD_MAX_POOLS_PER_FRAME: usize = 512;
/// Hard ceiling for env-provided candidate caps.
pub const HARD_MAX_CANDIDATES_PER_FRAME: usize = 64;
/// Hard ceiling for env-provided per-frame wall-clock budgets.
pub const HARD_MAX_TIME_BUDGET_MICROS: u64 = 20_000;

const Q96: u128 = 79_228_162_514_264_337_593_543_950_336u128;
const MIN_SQRT_RATIO: u128 = 4_295_128_739u128;
const FEE_DENOMINATOR: u128 = 1_000_000u128;

/// Pure marker carried through tests and runtime wiring to prove this lane is
/// observation-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoActionGuard;

impl NoActionGuard {
    /// Returns the only authorized mode label for this lane.
    pub const fn mode(self) -> &'static str {
        "dry-run-only"
    }
}

/// Returns true only for an explicit `1` opt-in.
pub fn enabled_from_value(value: Option<&std::ffi::OsStr>) -> bool {
    value.and_then(std::ffi::OsStr::to_str).is_some_and(|raw| raw.trim() == "1")
}

/// Runtime config for bounded frame evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunConfig {
    /// Maximum pools considered for one frame.
    pub max_pools_per_frame: usize,
    /// Maximum candidate loops emitted for one frame.
    pub max_candidates_per_frame: usize,
    /// Maximum wall-clock time spent per frame.
    pub time_budget: Duration,
    /// Input amount used for gross path estimation.
    pub amount_in_wei: u128,
}

impl Default for DryRunConfig {
    fn default() -> Self {
        Self {
            max_pools_per_frame: DEFAULT_MAX_POOLS_PER_FRAME,
            max_candidates_per_frame: DEFAULT_MAX_CANDIDATES_PER_FRAME,
            time_budget: Duration::from_micros(DEFAULT_TIME_BUDGET_MICROS),
            amount_in_wei: 1_000_000_000_000_000_000u128,
        }
    }
}

impl DryRunConfig {
    /// Clamp operator-provided env overrides to bounded ExEx-safe ceilings.
    pub fn clamped(mut self) -> Self {
        self.max_pools_per_frame = self.max_pools_per_frame.clamp(1, HARD_MAX_POOLS_PER_FRAME);
        self.max_candidates_per_frame =
            self.max_candidates_per_frame.clamp(1, HARD_MAX_CANDIDATES_PER_FRAME);
        let micros = u64::try_from(self.time_budget.as_micros()).unwrap_or(u64::MAX);
        self.time_budget = Duration::from_micros(micros.clamp(1, HARD_MAX_TIME_BUDGET_MICROS));
        self
    }
}

/// Protocol family supported by the dry-run graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Constant-product pool.
    UniswapV2,
    /// Aerodrome volatile pool (constant-product quote).
    AerodromeVolatile,
    /// Aerodrome stable pool (x^3y + y^3x invariant approximation by binary search).
    AerodromeStable,
    /// Concentrated-liquidity pool.
    UniswapV3,
}

impl Protocol {
    /// Stable wire spelling used in observation fields.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UniswapV2 => "uniswap_v2",
            Self::AerodromeVolatile => "aerodrome_volatile",
            Self::AerodromeStable => "aerodrome_stable",
            Self::UniswapV3 => "uniswap_v3",
        }
    }
}

/// Initialized V3 tick delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V3Tick {
    /// Tick index.
    pub tick: i32,
    /// Signed liquidity delta at the tick.
    pub liquidity_net: i128,
}

/// Snapshot of a pool used by the dry-run graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Pool address.
    pub pool: Address,
    /// Pool protocol.
    pub protocol: Protocol,
    /// First token.
    pub token0: Address,
    /// Second token.
    pub token1: Address,
    /// Token0 decimals.
    pub decimals0: u8,
    /// Token1 decimals.
    pub decimals1: u8,
    /// Fee in basis points for V2/Aerodrome, or pips for V3.
    pub fee: u32,
    /// Token0 reserve for V2/Aerodrome.
    pub reserve0: u128,
    /// Token1 reserve for V2/Aerodrome.
    pub reserve1: u128,
    /// V3 sqrt price Q64.96.
    pub sqrt_price_x96: Option<u128>,
    /// V3 active liquidity.
    pub liquidity: Option<u128>,
    /// V3 current tick.
    pub tick: Option<i32>,
    /// V3 tick spacing.
    pub tick_spacing: Option<i32>,
    /// V3 initialized ticks.
    pub ticks: Vec<V3Tick>,
}

impl PoolState {
    /// Creates a V2-like pool.
    pub fn v2_like(
        pool: Address,
        protocol: Protocol,
        token0: Address,
        token1: Address,
        fee_bps: u32,
        reserve0: u128,
        reserve1: u128,
    ) -> Self {
        Self {
            pool,
            protocol,
            token0,
            token1,
            decimals0: 18,
            decimals1: 18,
            fee: fee_bps,
            reserve0,
            reserve1,
            sqrt_price_x96: None,
            liquidity: None,
            tick: None,
            tick_spacing: None,
            ticks: Vec::new(),
        }
    }

    /// Creates a V3 pool.
    pub fn v3(
        pool: Address,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        sqrt_price_x96: u128,
        liquidity: u128,
        tick: i32,
        ticks: Vec<V3Tick>,
    ) -> Self {
        Self {
            pool,
            protocol: Protocol::UniswapV3,
            token0,
            token1,
            decimals0: 18,
            decimals1: 18,
            fee: fee_pips,
            reserve0: 0,
            reserve1: 0,
            sqrt_price_x96: Some(sqrt_price_x96),
            liquidity: Some(liquidity),
            tick: Some(tick),
            tick_spacing: Some(60),
            ticks,
        }
    }
}

/// Verified v3/Slipstream packed slot0 storage slot.
pub const V3_SLOT0_SLOT: u64 = 0;
/// Verified v3/Slipstream active-liquidity storage slot.
pub const V3_LIQUIDITY_SLOT: u64 = 4;
/// Verified `UniV2` packed reserve storage slot.
pub const UNIV2_RESERVES_SLOT: u64 = 8;
/// Verified Aerodrome volatile reserve0 storage slot.
pub const AERO_VOLATILE_RESERVE0_SLOT: u64 = 20;
/// Verified Aerodrome volatile reserve1 storage slot.
pub const AERO_VOLATILE_RESERVE1_SLOT: u64 = 21;

const MASK_160_BITS: usize = 160;
const MASK_128_BITS: usize = 128;
const MASK_112_BITS: usize = 112;
const MASK_24_BITS: usize = 24;
const TICK_SIGN_BIT: u32 = 1 << 23;
const TICK_MODULUS: i32 = 1 << 24;

/// Source of a live pool-state overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolOverlaySource {
    /// Overlay came from raw pool storage-slot diffs already produced by revm.
    SlotDiff,
    /// Overlay came from a bounded exact-state fallback read.
    StateProvider,
}

impl PoolOverlaySource {
    /// Stable label for diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SlotDiff => "slot-diff",
            Self::StateProvider => "state-provider",
        }
    }
}

/// Semantic live-state delta for a dirty pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStateDelta {
    /// Updated reserve0 for reserve pools.
    pub reserve0: Option<u128>,
    /// Updated reserve1 for reserve pools.
    pub reserve1: Option<u128>,
    /// Updated V3 sqrt price.
    pub sqrt_price_x96: Option<u128>,
    /// Updated V3 active liquidity.
    pub liquidity: Option<u128>,
    /// Updated V3 active tick.
    pub tick: Option<i32>,
    /// Overlay source.
    pub source: PoolOverlaySource,
    /// Block/payload epoch this delta belongs to.
    pub epoch: Option<u64>,
    /// True when all live fields required for this pool protocol were read and decoded.
    pub live_read_complete: bool,
    /// Honest overlay caveats/status labels.
    pub caveats: Vec<String>,
}

impl PoolStateDelta {
    /// Creates an empty delta for a source and epoch.
    pub const fn new(source: PoolOverlaySource, epoch: Option<u64>) -> Self {
        Self {
            reserve0: None,
            reserve1: None,
            sqrt_price_x96: None,
            liquidity: None,
            tick: None,
            source,
            epoch,
            live_read_complete: false,
            caveats: Vec::new(),
        }
    }

    /// Adds a caveat once, preserving first-seen order.
    pub fn add_caveat(&mut self, caveat: &str) {
        push_unique_caveat(&mut self.caveats, caveat);
    }

    /// True when this delta carries at least one field update.
    pub const fn has_state_update(&self) -> bool {
        self.reserve0.is_some()
            || self.reserve1.is_some()
            || self.sqrt_price_x96.is_some()
            || self.liquidity.is_some()
            || self.tick.is_some()
    }

    /// True when the live read included every field needed to quote this pool protocol.
    pub const fn is_live_read_complete(&self) -> bool {
        self.live_read_complete
    }

    /// Decodes a dirty pool's raw storage words using the verified TS overlay contract.
    pub fn from_slots(
        pool: &PoolState,
        slots: &BTreeMap<U256, U256>,
        source: PoolOverlaySource,
        epoch: Option<u64>,
        target_epoch: Option<u64>,
    ) -> Self {
        let mut delta = Self::new(source, epoch);
        if let (Some(actual), Some(target)) = (epoch, target_epoch)
            && actual != target
        {
            delta.add_caveat("overlay-epoch-mismatch");
            return delta;
        }
        if slots.is_empty() {
            delta.add_caveat("stale-baseline-fallback");
            return delta;
        }
        let mut saw_relevant_slot = false;
        let mut complete_live_read = false;
        match pool.protocol {
            Protocol::UniswapV3 => {
                let mut slot0_ok = false;
                let mut liquidity_ok = false;
                if let Some(word) = slots.get(&slot_key(V3_SLOT0_SLOT)) {
                    saw_relevant_slot = true;
                    if let Some((sqrt_price_x96, tick)) = decode_slot0(*word) {
                        if sqrt_price_x96 == 0 {
                            delta.add_caveat("overlay-decode-failed");
                        } else {
                            slot0_ok = true;
                            if pool.sqrt_price_x96 != Some(sqrt_price_x96)
                                || pool.tick != Some(tick)
                            {
                                delta.sqrt_price_x96 = Some(sqrt_price_x96);
                                delta.tick = Some(tick);
                            }
                        }
                    } else {
                        delta.add_caveat("overlay-decode-failed");
                    }
                }
                if let Some(word) = slots.get(&slot_key(V3_LIQUIDITY_SLOT)) {
                    saw_relevant_slot = true;
                    if let Some(liquidity) = u256_low_bits_to_u128(*word, MASK_128_BITS) {
                        if liquidity == 0 {
                            delta.add_caveat("overlay-decode-failed");
                        } else {
                            liquidity_ok = true;
                            if pool.liquidity != Some(liquidity) {
                                delta.liquidity = Some(liquidity);
                            }
                        }
                    } else {
                        delta.add_caveat("overlay-decode-failed");
                    }
                }
                complete_live_read = slot0_ok && liquidity_ok;
                if saw_relevant_slot && !complete_live_read && delta.caveats.is_empty() {
                    delta.add_caveat("partial-live-overlay");
                }
            }
            Protocol::UniswapV2 => {
                if let Some(word) = slots.get(&slot_key(UNIV2_RESERVES_SLOT)) {
                    saw_relevant_slot = true;
                    if let Some((reserve0, reserve1)) = decode_univ2_reserves(*word) {
                        if reserve0 == 0 || reserve1 == 0 {
                            delta.add_caveat("overlay-decode-failed");
                        } else {
                            complete_live_read = true;
                            if pool.reserve0 != reserve0 || pool.reserve1 != reserve1 {
                                delta.reserve0 = Some(reserve0);
                                delta.reserve1 = Some(reserve1);
                            }
                        }
                    } else {
                        delta.add_caveat("overlay-decode-failed");
                    }
                }
            }
            Protocol::AerodromeVolatile => {
                let reserve0 = slots.get(&slot_key(AERO_VOLATILE_RESERVE0_SLOT));
                let reserve1 = slots.get(&slot_key(AERO_VOLATILE_RESERVE1_SLOT));
                if reserve0.is_some() || reserve1.is_some() {
                    saw_relevant_slot = true;
                }
                match (reserve0, reserve1) {
                    (Some(word0), Some(word1)) => {
                        let decoded0 = u256_to_u128_checked(*word0);
                        let decoded1 = u256_to_u128_checked(*word1);
                        match (decoded0, decoded1) {
                            (Some(r0), Some(r1)) if r0 != 0 && r1 != 0 => {
                                complete_live_read = true;
                                if pool.reserve0 != r0 || pool.reserve1 != r1 {
                                    delta.reserve0 = Some(r0);
                                    delta.reserve1 = Some(r1);
                                }
                            }
                            _ => delta.add_caveat("overlay-decode-failed"),
                        }
                    }
                    (Some(_), None) | (None, Some(_)) => delta.add_caveat("partial-live-overlay"),
                    (None, None) => {}
                }
            }
            Protocol::AerodromeStable => {
                if !slots.is_empty() {
                    saw_relevant_slot = true;
                    delta.add_caveat("overlay-unsupported-protocol");
                }
            }
        }
        delta.live_read_complete = complete_live_read;
        if delta.live_read_complete {
            if delta.has_state_update() {
                delta.add_caveat("live-overlay-applied");
            } else if delta.caveats.is_empty() {
                delta.add_caveat("live-overlay-verified");
            }
        } else if !saw_relevant_slot && !slots.is_empty() && delta.caveats.is_empty() {
            delta.add_caveat("stale-baseline-fallback");
        }
        delta
    }

    /// Applies this delta to an owned pool copy. Returns true when a field changed.
    pub fn apply_to(&self, pool: &mut PoolState) -> bool {
        let mut changed = false;
        if let Some(reserve0) = self.reserve0 {
            changed |= pool.reserve0 != reserve0;
            pool.reserve0 = reserve0;
        }
        if let Some(reserve1) = self.reserve1 {
            changed |= pool.reserve1 != reserve1;
            pool.reserve1 = reserve1;
        }
        if let Some(sqrt_price_x96) = self.sqrt_price_x96 {
            changed |= pool.sqrt_price_x96 != Some(sqrt_price_x96);
            pool.sqrt_price_x96 = Some(sqrt_price_x96);
        }
        if let Some(liquidity) = self.liquidity {
            changed |= pool.liquidity != Some(liquidity);
            pool.liquidity = Some(liquidity);
        }
        if let Some(tick) = self.tick {
            changed |= pool.tick != Some(tick);
            pool.tick = Some(tick);
        }
        changed
    }
}

/// Returns the exact storage slots a fallback read should fetch for a pool.
pub const fn fallback_overlay_slots(pool: &PoolState) -> &'static [u64] {
    match pool.protocol {
        Protocol::UniswapV3 => &[V3_SLOT0_SLOT, V3_LIQUIDITY_SLOT],
        Protocol::UniswapV2 => &[UNIV2_RESERVES_SLOT],
        Protocol::AerodromeVolatile => &[AERO_VOLATILE_RESERVE0_SLOT, AERO_VOLATILE_RESERVE1_SLOT],
        Protocol::AerodromeStable => &[],
    }
}

/// Builds a storage slot key.
pub fn slot_key(slot: u64) -> U256 {
    U256::from(slot)
}

/// Composes semicolon-separated caveats in first-seen order.
pub fn compose_caveat_parts(parts: &[Option<&str>]) -> Option<String> {
    let mut caveats = Vec::new();
    for part in parts.iter().flatten() {
        for piece in part.split(';') {
            let trimmed = piece.trim();
            if !trimmed.is_empty() {
                push_unique_caveat(&mut caveats, trimmed);
            }
        }
    }
    (!caveats.is_empty()).then(|| caveats.join(";"))
}

fn push_unique_caveat(caveats: &mut Vec<String>, caveat: &str) {
    if !caveats.iter().any(|existing| existing == caveat) {
        caveats.push(caveat.to_string());
    }
}

fn u256_mask(bits: usize) -> U256 {
    (U256::from(1u64) << bits) - U256::from(1u64)
}

fn u256_low_bits_to_u128(value: U256, bits: usize) -> Option<u128> {
    u256_to_u128_checked(value & u256_mask(bits))
}

fn u256_to_u128_checked(value: U256) -> Option<u128> {
    if value > U256::from(u128::MAX) { None } else { Some(value.to::<u128>()) }
}

fn decode_slot0(word: U256) -> Option<(u128, i32)> {
    let sqrt_price_x96 = u256_low_bits_to_u128(word, MASK_160_BITS)?;
    let raw_tick = ((word >> MASK_160_BITS) & u256_mask(MASK_24_BITS)).to::<u32>();
    let tick = if raw_tick & TICK_SIGN_BIT != 0 {
        i32::try_from(raw_tick).ok()? - TICK_MODULUS
    } else {
        i32::try_from(raw_tick).ok()?
    };
    Some((sqrt_price_x96, tick))
}

fn decode_univ2_reserves(word: U256) -> Option<(u128, u128)> {
    let reserve0 = u256_low_bits_to_u128(word, MASK_112_BITS)?;
    let reserve1 = u256_low_bits_to_u128(word >> MASK_112_BITS, MASK_112_BITS)?;
    Some((reserve0, reserve1))
}

/// Quote result shared by all protocols.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteResult {
    /// Output amount.
    pub amount_out: u128,
    /// Input amount consumed.
    pub amount_in_consumed: u128,
    /// Whether a lower-confidence approximation was used.
    pub approximation: bool,
    /// Optional caveat for health reporting.
    pub caveat: Option<&'static str>,
    /// Confidence in [0, 1].
    pub confidence: f64,
}

/// Quote options matching the TS graph-arb executable quote guards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuoteOptions {
    /// True when token-in is known fee-on-transfer / non-standard.
    pub token_in_fee_on_transfer: bool,
}

/// V3 quote result for golden parity checks.
#[derive(Debug, Clone, PartialEq)]
pub struct V3QuoteResult {
    /// Output amount.
    pub amount_out: u128,
    /// Input amount consumed.
    pub amount_in_consumed: u128,
    /// Post-swap sqrt price.
    pub sqrt_price_x96_after: u128,
    /// Post-swap tick.
    pub tick_after: i32,
    /// Whether all reachable initialized ticks were crossed.
    pub crossed_all_ticks: bool,
    /// Confidence in [0, 1].
    pub confidence: f64,
}

/// Candidate loop returned by graph search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleCandidate {
    /// Canonical token sequence, without duplicate closing token.
    pub tokens: Vec<Address>,
    /// Pool sequence aligned with token hops.
    pub pools: Vec<Address>,
    /// Protocol sequence aligned with token hops.
    pub protocols: Vec<Protocol>,
    /// Lowercase 64-hex hash fingerprint.
    pub fingerprint: String,
    /// Candidate identifier derived from oriented tokens and pools.
    pub candidate_id: String,
    /// Gross estimated output.
    pub estimated_gross_wei: u128,
    /// Net estimate when a cost model exists; `None` prevents fabricated net=gross.
    pub estimated_net_wei: Option<u128>,
    /// Whether any hop is approximate.
    pub approximation: bool,
    /// Optional caveat.
    pub caveat: Option<String>,
}

/// Frame result produced by the bounded runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunFrame {
    /// Candidate observations.
    pub candidates: Vec<CycleCandidate>,
    /// Number of dirty pools considered.
    pub dirty_pool_count: usize,
    /// Whether work was truncated by bounds.
    pub truncated: bool,
    /// Health label.
    pub health: String,
    /// Runtime latency.
    pub latency_micros: u64,
    /// Optional caveat.
    pub caveat: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Edge {
    from: Address,
    to: Address,
    pool: Address,
    protocol: Protocol,
    rate: f64,
    approximation: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Graph {
    tokens: Vec<Address>,
    edges: Vec<Edge>,
}

/// Constant-product exact-input quote for V2 and Aerodrome volatile pools.
pub fn quote_v2_exact_in(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u32,
) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 || fee_bps >= 10_000 {
        return None;
    }
    let fee_den = BigUint::from(10_000u32);
    let amount_in_with_fee = BigUint::from(amount_in) * BigUint::from(10_000u32 - fee_bps);
    let numerator = &amount_in_with_fee * BigUint::from(reserve_out);
    let denominator = BigUint::from(reserve_in) * fee_den + amount_in_with_fee;
    big_to_u128(&(numerator / denominator))
}

/// Aerodrome volatile exact-input quote.
pub fn quote_aero_volatile_exact_in(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u32,
) -> Option<u128> {
    quote_v2_exact_in(amount_in, reserve_in, reserve_out, fee_bps)
}

/// Aerodrome stable exact-input quote using the Solidly invariant x^3y + y^3x.
pub fn quote_aero_stable_exact_in(
    amount_in: u128,
    reserve_in: u128,
    reserve_out: u128,
    fee_bps: u32,
) -> Option<u128> {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 || fee_bps >= 10_000 {
        return None;
    }
    let amount_in_after_fee = amount_in.saturating_mul(u128::from(10_000u32 - fee_bps)) / 10_000;
    let x0 = BigUint::from(reserve_in);
    let y0 = BigUint::from(reserve_out);
    let k = stable_k(&x0, &y0);
    let x1 = x0 + BigUint::from(amount_in_after_fee);
    let mut lo = BigUint::zero();
    let mut hi = y0.clone();
    while lo < hi {
        let mid = (&lo + &hi + BigUint::one()) >> 1usize;
        let y_after = &y0 - &mid;
        if stable_k(&x1, &y_after) >= k {
            lo = mid;
        } else {
            hi = mid - BigUint::one();
        }
    }
    big_to_u128(&lo)
}

/// Quotes one pool in the requested direction.
pub fn quote_pool_exact_in(
    pool: &PoolState,
    token_in: Address,
    amount_in: u128,
) -> Option<QuoteResult> {
    quote_pool_exact_in_with_options(pool, token_in, amount_in, QuoteOptions::default())
}

/// Quotes one pool with explicit parity guard options.
pub fn quote_pool_exact_in_with_options(
    pool: &PoolState,
    token_in: Address,
    amount_in: u128,
    options: QuoteOptions,
) -> Option<QuoteResult> {
    let zero_for_one = token_in == pool.token0;
    if !zero_for_one && token_in != pool.token1 {
        return None;
    }
    let quote = match pool.protocol {
        Protocol::UniswapV2 => quote_v2_exact_in(
            amount_in,
            if zero_for_one { pool.reserve0 } else { pool.reserve1 },
            if zero_for_one { pool.reserve1 } else { pool.reserve0 },
            pool.fee,
        )
        .map(|amount_out| QuoteResult {
            amount_out,
            amount_in_consumed: amount_in,
            approximation: false,
            caveat: None,
            confidence: 1.0,
        }),
        Protocol::AerodromeVolatile => quote_aero_volatile_exact_in(
            amount_in,
            if zero_for_one { pool.reserve0 } else { pool.reserve1 },
            if zero_for_one { pool.reserve1 } else { pool.reserve0 },
            pool.fee,
        )
        .map(|amount_out| QuoteResult {
            amount_out,
            amount_in_consumed: amount_in,
            approximation: false,
            caveat: None,
            confidence: 1.0,
        }),
        Protocol::AerodromeStable => quote_aero_stable_exact_in(
            amount_in,
            if zero_for_one { pool.reserve0 } else { pool.reserve1 },
            if zero_for_one { pool.reserve1 } else { pool.reserve0 },
            pool.fee,
        )
        .map(|amount_out| QuoteResult {
            amount_out,
            amount_in_consumed: amount_in,
            approximation: true,
            caveat: Some("stable-invariant-binary-search"),
            confidence: 0.5,
        }),
        Protocol::UniswapV3 => quote_v3_exact_in(
            pool.sqrt_price_x96?,
            pool.liquidity?,
            pool.tick?,
            pool.tick_spacing.unwrap_or(60),
            pool.fee,
            zero_for_one,
            amount_in,
            &pool.ticks,
        )
        .map(|r| QuoteResult {
            amount_out: r.amount_out,
            amount_in_consumed: r.amount_in_consumed,
            approximation: r.crossed_all_ticks,
            caveat: r.crossed_all_ticks.then_some("v3-price-limit"),
            confidence: r.confidence,
        }),
    }?;
    Some(apply_quote_options(quote, options))
}

fn apply_quote_options(mut quote: QuoteResult, options: QuoteOptions) -> QuoteResult {
    if options.token_in_fee_on_transfer {
        quote.approximation = true;
        quote.confidence = quote.confidence.min(0.5);
        quote.caveat = Some("fot_unmodeled");
    }
    quote
}

/// V3 Q64.96 exact-input quote for Phase 1 golden cases.
#[allow(clippy::too_many_arguments)]
pub fn quote_v3_exact_in(
    sqrt_price_x96: u128,
    liquidity: u128,
    tick: i32,
    _tick_spacing: i32,
    fee_pips: u32,
    zero_for_one: bool,
    amount_in: u128,
    ticks: &[V3Tick],
) -> Option<V3QuoteResult> {
    if sqrt_price_x96 <= MIN_SQRT_RATIO || liquidity == 0 || amount_in == 0 || fee_pips >= 1_000_000
    {
        return None;
    }

    let mut current_sqrt = BigUint::from(sqrt_price_x96);
    let mut current_tick = tick;
    let mut current_liquidity = BigInt::from(liquidity);
    let mut amount_remaining = BigUint::from(amount_in);
    let mut amount_out = BigUint::zero();
    let mut crossed_all_ticks = false;
    let mut initialized = ticks.to_vec();
    initialized.sort_by(|a, b| a.tick.cmp(&b.tick));

    while !amount_remaining.is_zero() {
        let Some(liquidity_u) = current_liquidity.to_biguint() else {
            crossed_all_ticks = true;
            break;
        };
        if liquidity_u.is_zero() {
            crossed_all_ticks = true;
            break;
        }
        let next_tick = if zero_for_one {
            initialized.iter().rev().find(|t| t.tick < current_tick).map(|t| t.tick)
        } else {
            initialized.iter().find(|t| t.tick > current_tick).map(|t| t.tick)
        };
        let target_sqrt = next_tick.and_then(get_sqrt_ratio_at_tick).unwrap_or_else(|| {
            if zero_for_one {
                BigUint::from(MIN_SQRT_RATIO + 1)
            } else {
                max_sqrt_ratio().unwrap_or_else(|| BigUint::from(u128::MAX)) - BigUint::one()
            }
        });
        let step = compute_swap_step(
            &current_sqrt,
            &target_sqrt,
            &liquidity_u,
            &amount_remaining,
            fee_pips,
            zero_for_one,
        )?;
        amount_remaining -= &step.amount_in + &step.fee_amount;
        amount_out += &step.amount_out;
        current_sqrt = step.sqrt_next;
        if let Some(nt) = next_tick {
            if current_sqrt == target_sqrt {
                let delta =
                    initialized.iter().find(|t| t.tick == nt).map_or(0, |t| t.liquidity_net);
                current_liquidity = if zero_for_one {
                    current_liquidity - BigInt::from(delta)
                } else {
                    current_liquidity + BigInt::from(delta)
                };
                current_tick = if zero_for_one { nt - 1 } else { nt };
                continue;
            }
        }
        current_tick = get_tick_at_sqrt_ratio(&current_sqrt).unwrap_or(current_tick);
        if step.amount_in.is_zero() && step.fee_amount.is_zero() {
            crossed_all_ticks = true;
            break;
        }
    }

    let consumed = BigUint::from(amount_in) - amount_remaining;
    Some(V3QuoteResult {
        amount_out: big_to_u128(&amount_out)?,
        amount_in_consumed: big_to_u128(&consumed)?,
        sqrt_price_x96_after: big_to_u128(&current_sqrt)?,
        tick_after: current_tick,
        crossed_all_ticks,
        confidence: if crossed_all_ticks { 0.5 } else { 1.0 },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwapStep {
    sqrt_next: BigUint,
    amount_in: BigUint,
    amount_out: BigUint,
    fee_amount: BigUint,
}

fn compute_swap_step(
    sqrt_current: &BigUint,
    sqrt_target: &BigUint,
    liquidity: &BigUint,
    amount_remaining: &BigUint,
    fee_pips: u32,
    zero_for_one: bool,
) -> Option<SwapStep> {
    let fee_complement = BigUint::from(FEE_DENOMINATOR - u128::from(fee_pips));
    let amount_remaining_less_fee =
        amount_remaining * &fee_complement / BigUint::from(FEE_DENOMINATOR);
    let amount_in_to_target = if zero_for_one {
        get_amount0_delta(sqrt_target, sqrt_current, liquidity, true)
    } else {
        get_amount1_delta(sqrt_current, sqrt_target, liquidity, true)
    };
    let max = amount_remaining_less_fee >= amount_in_to_target;
    let sqrt_next = if max {
        sqrt_target.clone()
    } else if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(
            sqrt_current,
            liquidity,
            &amount_remaining_less_fee,
        )?
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(
            sqrt_current,
            liquidity,
            &amount_remaining_less_fee,
        )
    };
    let amount_in_used = if zero_for_one {
        get_amount0_delta(&sqrt_next, sqrt_current, liquidity, true)
    } else {
        get_amount1_delta(sqrt_current, &sqrt_next, liquidity, true)
    };
    let amount_out = if zero_for_one {
        get_amount1_delta(&sqrt_next, sqrt_current, liquidity, false)
    } else {
        get_amount0_delta(sqrt_current, &sqrt_next, liquidity, false)
    };
    let fee_amount = if max {
        mul_div_rounding_up(&amount_in_used, &BigUint::from(fee_pips), &fee_complement)
    } else {
        amount_remaining - &amount_in_used
    };
    Some(SwapStep { sqrt_next, amount_in: amount_in_used, amount_out, fee_amount })
}

fn get_amount0_delta(a: &BigUint, b: &BigUint, liquidity: &BigUint, round_up: bool) -> BigUint {
    let (lower, upper) = if a <= b { (a, b) } else { (b, a) };
    let numerator1 = liquidity << 96usize;
    let numerator2 = upper - lower;
    if round_up {
        div_rounding_up(&mul_div_rounding_up(&numerator1, &numerator2, upper), lower)
    } else {
        (&numerator1 * numerator2 / upper) / lower
    }
}

fn get_amount1_delta(a: &BigUint, b: &BigUint, liquidity: &BigUint, round_up: bool) -> BigUint {
    let (lower, upper) = if a <= b { (a, b) } else { (b, a) };
    let numerator = liquidity * (upper - lower);
    if round_up {
        div_rounding_up(&numerator, &BigUint::from(Q96))
    } else {
        numerator / BigUint::from(Q96)
    }
}

fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_price: &BigUint,
    liquidity: &BigUint,
    amount: &BigUint,
) -> Option<BigUint> {
    let numerator1 = liquidity << 96usize;
    let product = amount * sqrt_price;
    let denominator = &numerator1 + product;
    Some(mul_div_rounding_up(&numerator1, sqrt_price, &denominator))
}

fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_price: &BigUint,
    liquidity: &BigUint,
    amount: &BigUint,
) -> BigUint {
    sqrt_price + (amount << 96usize) / liquidity
}

fn mul_div_rounding_up(a: &BigUint, b: &BigUint, denominator: &BigUint) -> BigUint {
    div_rounding_up(&(a * b), denominator)
}

fn div_rounding_up(numerator: &BigUint, denominator: &BigUint) -> BigUint {
    let q = numerator / denominator;
    if numerator % denominator == BigUint::zero() { q } else { q + BigUint::one() }
}

const MIN_TICK: i32 = -887_272;
const MAX_TICK: i32 = 887_272;

fn get_sqrt_ratio_at_tick(tick: i32) -> Option<BigUint> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return None;
    }
    let abs_tick = tick.unsigned_abs();
    let mut ratio = if abs_tick & 0x1 != 0 {
        hex_biguint("fffcb933bd6fad37aa2d162d1a594001")?
    } else {
        BigUint::one() << 128usize
    };
    for (bit, factor) in [
        (0x2, "fff97272373d413259a46990580e213a"),
        (0x4, "fff2e50f5f656932ef12357cf3c7fdcc"),
        (0x8, "ffe5caca7e10e4e61c3624eaa0941cd0"),
        (0x10, "ffcb9843d60f6159c9db58835c926644"),
        (0x20, "ff973b41fa98c081472e6896dfb254c0"),
        (0x40, "ff2ea16466c96a3843ec78b326b52861"),
        (0x80, "fe5dee046a99a2a811c461f1969c3053"),
        (0x100, "fcbe86c7900a88aedcffc83b479aa3a4"),
        (0x200, "f987a7253ac413176f2b074cf7815e54"),
        (0x400, "f3392b0822b70005940c7a398e4b70f3"),
        (0x800, "e7159475a2c29b7443b29c7fa6e889d9"),
        (0x1000, "d097f3bdfd2022b8845ad8f792aa5825"),
        (0x2000, "a9f746462d870fdf8a65dc1f90e061e5"),
        (0x4000, "70d869a156d2a1b890bb3df62baf32f7"),
        (0x8000, "31be135f97d08fd981231505542fcfa6"),
        (0x10000, "9aa508b5b7a84e1c677de54f3e99bc9"),
        (0x20000, "5d6af8dedb81196699c329225ee604"),
        (0x40000, "2216e584f5fa1ea926041bedfe98"),
        (0x80000, "48a170391f7dc42444e8fa2"),
    ] {
        if abs_tick & bit != 0 {
            ratio = (ratio * hex_biguint(factor)?) >> 128usize;
        }
    }
    if tick > 0 {
        ratio = max_uint256() / ratio;
    }
    let q32 = BigUint::one() << 32usize;
    let mut sqrt_price_x96 = &ratio >> 32usize;
    if ratio % &q32 != BigUint::zero() {
        sqrt_price_x96 += BigUint::one();
    }
    Some(sqrt_price_x96)
}

fn get_tick_at_sqrt_ratio(sqrt_price: &BigUint) -> Option<i32> {
    let min = get_sqrt_ratio_at_tick(MIN_TICK)?;
    let max = get_sqrt_ratio_at_tick(MAX_TICK)?;
    if sqrt_price < &min || sqrt_price >= &max {
        return None;
    }
    let mut lo = MIN_TICK;
    let mut hi = MAX_TICK;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if get_sqrt_ratio_at_tick(mid)? <= *sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

fn max_uint256() -> BigUint {
    (BigUint::one() << 256usize) - BigUint::one()
}

fn max_sqrt_ratio() -> Option<BigUint> {
    get_sqrt_ratio_at_tick(MAX_TICK)
}

fn hex_biguint(hex: &str) -> Option<BigUint> {
    BigUint::parse_bytes(hex.as_bytes(), 16)
}

fn stable_k(x: &BigUint, y: &BigUint) -> BigUint {
    let x2 = x * x;
    let y2 = y * y;
    (&x2 * x * y) + (x * &y2 * y)
}

/// Builds dirty-pool subset from a candidate-index reverse map.
pub fn dirty_pool_subset<'a>(
    pools: &'a [PoolState],
    dirty_pools: &BTreeSet<Address>,
    reverse: &HashMap<Address, Vec<usize>>,
) -> (Vec<&'a PoolState>, bool) {
    if dirty_pools.is_empty() {
        return (Vec::new(), false);
    }
    let mut indexes = BTreeSet::new();
    for pool in dirty_pools {
        if let Some(values) = reverse.get(pool) {
            indexes.extend(values.iter().copied());
        }
    }
    let subset = indexes.into_iter().filter_map(|i| pools.get(i)).collect();
    (subset, false)
}

/// Load an operator-supplied decoded pool baseline from JSON.
///
/// This keeps Rust free of Node.js/Postgres dependencies: base-mev or another
/// offline process owns registry construction, while the ExEx consumes only a
/// static file of `PoolState` rows when operators explicitly provide one.
pub fn load_pool_baseline_from_path(path: impl AsRef<Path>) -> Result<Vec<PoolState>, String> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|err| {
        format!("failed to read arb dry-run pool baseline {}: {err}", path.display())
    })?;
    serde_json::from_str::<Vec<PoolState>>(&raw).map_err(|err| {
        format!("failed to parse arb dry-run pool baseline {}: {err}", path.display())
    })
}

/// Creates a reverse map from pool address to candidate index positions.
pub fn candidate_index_reverse_map(pools: &[PoolState]) -> HashMap<Address, Vec<usize>> {
    let mut reverse: HashMap<Address, Vec<usize>> = HashMap::new();
    for (i, pool) in pools.iter().enumerate() {
        reverse.entry(pool.pool).or_default().push(i);
    }
    reverse
}

/// Runs a bounded per-frame dry-run.
pub fn run_frame(
    pools: &[PoolState],
    dirty_pools: &BTreeSet<Address>,
    config: &DryRunConfig,
    guard: NoActionGuard,
) -> DryRunFrame {
    run_frame_internal(pools, dirty_pools, &BTreeMap::new(), config, guard, false)
}

/// Runs a bounded per-frame dry-run with live dirty-pool state overlay.
pub fn run_frame_with_overlay(
    pools: &[PoolState],
    dirty_pools: &BTreeSet<Address>,
    dirty_state: &BTreeMap<Address, PoolStateDelta>,
    config: &DryRunConfig,
    guard: NoActionGuard,
) -> DryRunFrame {
    run_frame_internal(pools, dirty_pools, dirty_state, config, guard, true)
}

fn run_frame_internal(
    pools: &[PoolState],
    dirty_pools: &BTreeSet<Address>,
    dirty_state: &BTreeMap<Address, PoolStateDelta>,
    config: &DryRunConfig,
    guard: NoActionGuard,
    overlay_enabled: bool,
) -> DryRunFrame {
    let started = Instant::now();
    let reverse = candidate_index_reverse_map(pools);
    let mut all_dirty = dirty_pools.clone();
    all_dirty.extend(dirty_state.keys().copied());
    let mut truncated = false;
    let mut caveats = Vec::new();
    if pools.is_empty() {
        return DryRunFrame {
            candidates: Vec::new(),
            dirty_pool_count: all_dirty.len(),
            truncated: false,
            health: "unsupported".to_string(),
            latency_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            caveat: Some("pool-baseline-unavailable-in-rust-phase1".to_string()),
        };
    }
    if !all_dirty.is_empty() && !all_dirty.iter().any(|pool| reverse.contains_key(pool)) {
        return DryRunFrame {
            candidates: Vec::new(),
            dirty_pool_count: all_dirty.len(),
            truncated: false,
            health: "skipped".to_string(),
            latency_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            caveat: Some("dirty-pool-not-in-baseline".to_string()),
        };
    }

    let (selected_indexes, dirty_indexes_len) = select_frame_indexes(pools, &all_dirty, &reverse);
    let mut bounded_indexes = selected_indexes;
    if bounded_indexes.len() > config.max_pools_per_frame {
        if dirty_indexes_len > config.max_pools_per_frame {
            push_unique_caveat(&mut caveats, "dirty-pool-outside-frame-cap");
        } else {
            push_unique_caveat(&mut caveats, "bounded-frame-truncated");
        }
        bounded_indexes.truncate(config.max_pools_per_frame);
        truncated = true;
    }

    let mut owned: Vec<PoolState> =
        bounded_indexes.into_iter().filter_map(|i| pools.get(i).cloned()).collect();
    if overlay_enabled {
        for pool in &mut owned {
            if let Some(delta) = dirty_state.get(&pool.pool) {
                for caveat in &delta.caveats {
                    push_unique_caveat(&mut caveats, caveat);
                }
                delta.apply_to(pool);
            }
        }
    }

    let (mut candidates, timed_out) = find_negative_cycle_candidates_bounded(
        &owned,
        config.amount_in_wei,
        started,
        config.time_budget,
        config.max_candidates_per_frame + 1,
    );
    if !all_dirty.is_empty() {
        candidates.retain(|candidate| candidate.pools.iter().any(|pool| all_dirty.contains(pool)));
    }
    if candidates.len() > config.max_candidates_per_frame {
        candidates.truncate(config.max_candidates_per_frame);
        truncated = true;
        push_unique_caveat(&mut caveats, "bounded-frame-truncated");
    }
    if timed_out {
        truncated = true;
        push_unique_caveat(&mut caveats, "bounded-frame-truncated");
    }
    if started.elapsed() > config.time_budget {
        truncated = true;
        push_unique_caveat(&mut caveats, "bounded-frame-truncated");
    }
    if candidates.is_empty()
        && !all_dirty.is_empty()
        && !caveats.iter().any(|c| c == "dirty-pool-outside-frame-cap")
    {
        push_unique_caveat(&mut caveats, "no-candidates-for-dirty-pools");
    }
    let health = if guard.mode() != "dry-run-only" {
        "error"
    } else if truncated {
        "truncated"
    } else {
        "ok"
    }
    .to_string();
    DryRunFrame {
        candidates,
        dirty_pool_count: all_dirty.len(),
        truncated,
        health,
        latency_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        caveat: compose_owned_caveats(&caveats),
    }
}

fn select_frame_indexes(
    pools: &[PoolState],
    dirty_pools: &BTreeSet<Address>,
    reverse: &HashMap<Address, Vec<usize>>,
) -> (Vec<usize>, usize) {
    if dirty_pools.is_empty() {
        return ((0..pools.len()).collect(), 0);
    }
    let mut dirty_indexes = BTreeSet::new();
    for pool in dirty_pools {
        if let Some(values) = reverse.get(pool) {
            dirty_indexes.extend(values.iter().copied());
        }
    }
    let mut selected = dirty_indexes.iter().copied().collect::<Vec<_>>();
    let dirty_len = selected.len();
    selected.extend((0..pools.len()).filter(|i| !dirty_indexes.contains(i)));
    (selected, dirty_len)
}

fn compose_owned_caveats(caveats: &[String]) -> Option<String> {
    let mut out = Vec::new();
    for caveat in caveats {
        for piece in caveat.split(';') {
            let trimmed = piece.trim();
            if !trimmed.is_empty() {
                push_unique_caveat(&mut out, trimmed);
            }
        }
    }
    (!out.is_empty()).then(|| out.join(";"))
}

/// Builds graph and returns canonical negative-cycle candidates.
pub fn find_negative_cycle_candidates(pools: &[PoolState], amount_in: u128) -> Vec<CycleCandidate> {
    find_negative_cycle_candidates_bounded(
        pools,
        amount_in,
        Instant::now(),
        Duration::from_secs(u64::MAX / 2),
        usize::MAX,
    )
    .0
}

fn find_negative_cycle_candidates_bounded(
    pools: &[PoolState],
    amount_in: u128,
    started: Instant,
    budget: Duration,
    max_candidates: usize,
) -> (Vec<CycleCandidate>, bool) {
    let graph = build_graph(pools);
    let (cycles, mut timed_out) = find_negative_cycles(&graph, started, budget, max_candidates);
    let mut out = Vec::new();
    for cycle in cycles {
        if started.elapsed() >= budget {
            timed_out = true;
            break;
        }
        if let Some(candidate) = cycle_to_candidate(&cycle, amount_in, pools) {
            if !out.iter().any(|c: &CycleCandidate| c.fingerprint == candidate.fingerprint) {
                out.push(candidate);
                if out.len() >= max_candidates {
                    break;
                }
            }
        }
    }
    out.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
    (out, timed_out)
}

fn build_graph(pools: &[PoolState]) -> Graph {
    let mut token_set = BTreeSet::new();
    let mut edges = Vec::new();
    for pool in pools {
        token_set.insert(pool.token0);
        token_set.insert(pool.token1);
        if let Some(rate) = marginal_rate(pool, true) {
            edges.push(Edge {
                from: pool.token0,
                to: pool.token1,
                pool: pool.pool,
                protocol: pool.protocol,
                rate,
                approximation: matches!(
                    pool.protocol,
                    Protocol::UniswapV3 | Protocol::AerodromeStable
                ),
            });
        }
        if let Some(rate) = marginal_rate(pool, false) {
            edges.push(Edge {
                from: pool.token1,
                to: pool.token0,
                pool: pool.pool,
                protocol: pool.protocol,
                rate,
                approximation: matches!(
                    pool.protocol,
                    Protocol::UniswapV3 | Protocol::AerodromeStable
                ),
            });
        }
    }
    Graph { tokens: token_set.into_iter().collect(), edges }
}

fn marginal_rate(pool: &PoolState, zero_for_one: bool) -> Option<f64> {
    let fee = match pool.protocol {
        Protocol::UniswapV3 => {
            if pool.fee >= FEE_DENOMINATOR as u32 {
                return None;
            }
            (FEE_DENOMINATOR - u128::from(pool.fee)) as f64 / FEE_DENOMINATOR as f64
        }
        Protocol::UniswapV2 | Protocol::AerodromeVolatile | Protocol::AerodromeStable => {
            if pool.fee >= 10_000 {
                return None;
            }
            (10_000 - pool.fee) as f64 / 10_000.0
        }
    };

    match pool.protocol {
        Protocol::UniswapV2 | Protocol::AerodromeVolatile | Protocol::AerodromeStable => {
            let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero_for_one {
                (pool.reserve0, pool.reserve1, pool.decimals0, pool.decimals1)
            } else {
                (pool.reserve1, pool.reserve0, pool.decimals1, pool.decimals0)
            };
            if reserve_in == 0 || reserve_out == 0 {
                return None;
            }
            let human_in = human_amount(reserve_in, decimals_in);
            let human_out = human_amount(reserve_out, decimals_out);
            if !(human_in > 0.0 && human_out > 0.0) {
                return None;
            }
            let spot = if pool.protocol == Protocol::AerodromeStable {
                (human_out * (3.0 * human_in * human_in + human_out * human_out))
                    / (human_in * (human_in * human_in + 3.0 * human_out * human_out))
            } else {
                human_out / human_in
            };
            (spot > 0.0 && spot.is_finite()).then_some(spot * fee)
        }
        Protocol::UniswapV3 => {
            let sqrt = pool.sqrt_price_x96? as f64 / Q96 as f64;
            let mut price = sqrt * sqrt;
            price *= 10f64.powi(i32::from(pool.decimals0) - i32::from(pool.decimals1));
            if !(price > 0.0 && price.is_finite()) {
                return None;
            }
            Some(if zero_for_one { price * fee } else { (1.0 / price) * fee })
        }
    }
}

fn human_amount(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(i32::from(decimals))
}

fn find_negative_cycles(
    graph: &Graph,
    started: Instant,
    budget: Duration,
    max_cycles: usize,
) -> (Vec<Vec<Edge>>, bool) {
    let mut cycles = Vec::new();
    let mut timed_out = false;
    for start in &graph.tokens {
        if started.elapsed() >= budget || cycles.len() >= max_cycles {
            timed_out = started.elapsed() >= budget;
            break;
        }
        if dfs_cycles(
            graph,
            *start,
            *start,
            &mut Vec::new(),
            &mut HashSet::new(),
            &mut cycles,
            4,
            started,
            budget,
            max_cycles,
        ) {
            timed_out = true;
            break;
        }
    }
    (cycles, timed_out)
}

fn dfs_cycles(
    graph: &Graph,
    start: Address,
    current: Address,
    path: &mut Vec<Edge>,
    seen: &mut HashSet<Address>,
    cycles: &mut Vec<Vec<Edge>>,
    max_hops: usize,
    started: Instant,
    budget: Duration,
    max_cycles: usize,
) -> bool {
    if started.elapsed() >= budget {
        return true;
    }
    if path.len() >= max_hops || cycles.len() >= max_cycles {
        return false;
    }
    seen.insert(current);
    for edge in graph.edges.iter().filter(|e| e.from == current) {
        if started.elapsed() >= budget {
            seen.remove(&current);
            return true;
        }
        if edge.to == start && !path.is_empty() {
            let mut closed = path.clone();
            closed.push(edge.clone());
            let product = closed.iter().fold(1.0, |acc, e| acc * e.rate);
            if product > 1.0000001 {
                cycles.push(canonicalize_cycle(&closed));
                if cycles.len() >= max_cycles {
                    break;
                }
            }
        } else if !seen.contains(&edge.to) {
            path.push(edge.clone());
            let timed_out = dfs_cycles(
                graph, start, edge.to, path, seen, cycles, max_hops, started, budget, max_cycles,
            );
            path.pop();
            if timed_out {
                seen.remove(&current);
                return true;
            }
        }
    }
    seen.remove(&current);
    false
}

fn canonicalize_cycle(edges: &[Edge]) -> Vec<Edge> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut rotations = Vec::new();
    for i in 0..edges.len() {
        let mut rotated = edges[i..].to_vec();
        rotated.extend_from_slice(&edges[..i]);
        rotations.push(rotated);
    }
    rotations.sort_by(|a, b| edge_fingerprint(a).cmp(&edge_fingerprint(b)));
    rotations.remove(0)
}

fn edge_fingerprint(edges: &[Edge]) -> String {
    edges
        .iter()
        .map(|e| format!("{:#x}->{:#x}:{:#x}", e.from, e.to, e.pool))
        .collect::<Vec<_>>()
        .join("|")
}

fn edge_fingerprint_hash(edge_fingerprint: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = keccak256(edge_fingerprint.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.0 {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn cycle_to_candidate(
    edges: &[Edge],
    amount_in: u128,
    pools: &[PoolState],
) -> Option<CycleCandidate> {
    let mut amount = amount_in;
    let mut approximation = false;
    let mut caveats = Vec::new();
    for edge in edges {
        let pool = pools.iter().find(|p| p.pool == edge.pool)?;
        let quote = quote_pool_exact_in(pool, edge.from, amount)?;
        amount = quote.amount_out;
        approximation |= edge.approximation || quote.approximation;
        if let Some(c) = quote.caveat {
            caveats.push(c.to_string());
        }
    }
    let tokens = edges.iter().map(|e| e.from).collect::<Vec<_>>();
    let pool_ids = edges.iter().map(|e| e.pool).collect::<Vec<_>>();
    let protocols = edges.iter().map(|e| e.protocol).collect::<Vec<_>>();
    let candidate_id = edge_fingerprint(edges);
    let fingerprint = edge_fingerprint_hash(&candidate_id);
    Some(CycleCandidate {
        candidate_id,
        fingerprint,
        tokens,
        pools: pool_ids,
        protocols,
        estimated_gross_wei: amount,
        estimated_net_wei: None,
        approximation,
        caveat: (!caveats.is_empty()).then(|| caveats.join(",")),
    })
}

fn big_to_u128(value: &BigUint) -> Option<u128> {
    value.to_u128()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::path::PathBuf;

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[derive(Debug, Deserialize)]
    struct ParityCorpus {
        #[serde(rename = "quoteCases")]
        quote_cases: Vec<CorpusQuoteCase>,
    }

    #[derive(Debug, Deserialize)]
    struct CorpusQuoteCase {
        name: String,
        #[serde(rename = "tokenIn")]
        token_in: Address,
        #[serde(rename = "amountIn")]
        amount_in: String,
        pool: CorpusPool,
        expected: CorpusExpected,
    }

    #[derive(Debug, Deserialize)]
    struct CorpusExpected {
        #[serde(rename = "amountOut")]
        amount_out: String,
        confidence: Option<f64>,
        approximation: Option<String>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct CorpusPool {
        pool: Address,
        protocol: Protocol,
        token0: Address,
        token1: Address,
        decimals0: u8,
        decimals1: u8,
        #[serde(rename = "feeBps")]
        fee_bps: u32,
        reserve0: Option<String>,
        reserve1: Option<String>,
        #[serde(rename = "sqrtPriceX96")]
        sqrt_price_x96: Option<String>,
        liquidity: Option<String>,
        tick: Option<i32>,
        #[serde(rename = "tickSpacing")]
        tick_spacing: Option<i32>,
        #[serde(default)]
        ticks: Vec<CorpusTick>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct CorpusTick {
        tick: i32,
        #[serde(rename = "liquidityNet")]
        liquidity_net: String,
    }

    impl CorpusPool {
        fn into_pool_state(self) -> PoolState {
            let protocol = self.protocol;
            PoolState {
                pool: self.pool,
                protocol,
                token0: self.token0,
                token1: self.token1,
                decimals0: self.decimals0,
                decimals1: self.decimals1,
                fee: if protocol == Protocol::UniswapV3 {
                    self.fee_bps * 100
                } else {
                    self.fee_bps
                },
                reserve0: parse_u128_opt(self.reserve0.as_deref()).unwrap_or(0),
                reserve1: parse_u128_opt(self.reserve1.as_deref()).unwrap_or(0),
                sqrt_price_x96: parse_u128_opt(self.sqrt_price_x96.as_deref()),
                liquidity: parse_u128_opt(self.liquidity.as_deref()),
                tick: self.tick,
                tick_spacing: self.tick_spacing,
                ticks: self
                    .ticks
                    .into_iter()
                    .map(|tick| V3Tick {
                        tick: tick.tick,
                        liquidity_net: tick.liquidity_net.parse::<i128>().unwrap(),
                    })
                    .collect(),
            }
        }
    }

    fn parse_u128_opt(raw: Option<&str>) -> Option<u128> {
        raw.map(|value| value.parse::<u128>().unwrap())
    }

    fn load_parity_corpus() -> ParityCorpus {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("BASE_MEV_GRAPH_ARB_PARITY_CORPUS") {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(manifest.join("fixtures/graph-arb-parity-corpus.json"));
        let path = candidates
            .into_iter()
            .find(|path| path.exists())
            .expect("graph-arb parity corpus fixture not found");
        let raw = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn load_pool_baseline_sample_fixture() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("BASE_MEV_ARB_DRYRUN_BASELINE_SAMPLE") {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(manifest.join("fixtures/arb-dryrun/pool-baseline.sample.json"));
        candidates
            .into_iter()
            .find(|path| path.exists())
            .expect("arb dry-run pool baseline sample fixture not found")
    }

    #[test]
    fn v2_quote_matches_constant_product_literal() {
        let out = quote_v2_exact_in(10, 1_000, 1_100, 1).unwrap();
        assert_eq!(out, 10);
        let out_large = quote_v2_exact_in(
            10u128.pow(18),
            1_000u128 * 10u128.pow(18),
            1_100u128 * 10u128.pow(18),
            1,
        )
        .unwrap();
        assert_eq!(out_large, 1_098_791_318_560_571_284);
    }

    #[test]
    fn cross_decimal_marginal_rate_uses_human_units() {
        let mut pool = PoolState::v2_like(
            addr(0xa1),
            Protocol::UniswapV2,
            addr(0x11),
            addr(0x22),
            0,
            100 * 10u128.pow(18),
            220_000 * 10u128.pow(6),
        );
        pool.decimals0 = 18;
        pool.decimals1 = 6;

        let weth_to_usdc = marginal_rate(&pool, true).unwrap();
        let usdc_to_weth = marginal_rate(&pool, false).unwrap();
        assert!((weth_to_usdc - 2_200.0).abs() < 1e-9);
        assert!((usdc_to_weth - (1.0 / 2_200.0)).abs() < 1e-15);

        let mut cheaper = pool.clone();
        cheaper.pool = addr(0xa2);
        cheaper.reserve1 = 200_000 * 10u128.pow(6);
        let candidates = find_negative_cycle_candidates(&[pool, cheaper], 10u128.pow(18));
        assert!(!candidates.is_empty());
    }

    #[test]
    fn aero_volatile_reuses_v2_math() {
        assert_eq!(
            quote_aero_volatile_exact_in(1_000, 10_000, 20_000, 30),
            quote_v2_exact_in(1_000, 10_000, 20_000, 30)
        );
    }

    #[test]
    fn aero_stable_preserves_low_slippage_near_parity() {
        let out =
            quote_aero_stable_exact_in(1_000_000, 1_000_000_000_000, 1_000_000_000_000, 4).unwrap();
        assert!(out > 998_000);
        assert!(out <= 1_000_000);
    }

    #[test]
    fn v3_single_tick_matches_ts_golden() {
        let r = quote_v3_exact_in(
            Q96,
            10u128.pow(18),
            0,
            60,
            3000,
            true,
            10u128.pow(15),
            &[
                V3Tick { tick: -887220, liquidity_net: 10i128.pow(18) },
                V3Tick { tick: 887220, liquidity_net: -10i128.pow(18) },
            ],
        )
        .unwrap();
        assert_eq!(r.amount_out, 996_006_981_039_903);
        assert_eq!(r.amount_in_consumed, 10u128.pow(15));
        assert_eq!(r.sqrt_price_x96_after, 79_149_250_711_305_166_342_700_278_159);
        assert_eq!(r.tick_after, -20);
        assert!(!r.crossed_all_ticks);
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn v3_multi_tick_matches_ts_golden() {
        let r = quote_v3_exact_in(
            Q96,
            10u128.pow(18),
            0,
            60,
            3000,
            true,
            5 * 10u128.pow(16),
            &[
                V3Tick { tick: -60, liquidity_net: 5 * 10i128.pow(17) },
                V3Tick { tick: 60, liquidity_net: -(5 * 10i128.pow(17)) },
                V3Tick { tick: -887220, liquidity_net: 10i128.pow(18) },
                V3Tick { tick: 887220, liquidity_net: -10i128.pow(18) },
            ],
        )
        .unwrap();
        assert_eq!(r.amount_out, 45_582_673_299_480_711);
        assert_eq!(r.sqrt_price_x96_after, 72_242_616_087_487_392_683_269_770_548);
        assert_eq!(r.tick_after, -1847);
        assert!(!r.crossed_all_ticks);
    }

    #[test]
    fn loads_ts_issue76_and_fot_parity_corpus() {
        let corpus = load_parity_corpus();

        let issue76 = corpus
            .quote_cases
            .iter()
            .find(|case| case.name == "issue76-v3-bitcoin-usdc-engine-golden")
            .expect("issue76 quote case");
        let issue76_pool = issue76.pool.clone().into_pool_state();
        let issue76_amount = issue76.amount_in.parse::<u128>().unwrap();
        let issue76_quote =
            quote_pool_exact_in(&issue76_pool, issue76.token_in, issue76_amount).unwrap();
        assert_eq!(issue76_quote.amount_out.to_string(), issue76.expected.amount_out);
        assert_eq!(issue76_quote.confidence, issue76.expected.confidence.unwrap());
        assert!(!issue76_quote.approximation);

        let fot = corpus
            .quote_cases
            .iter()
            .find(|case| case.name == "fot-marker-preserves-amount-downgrades-confidence")
            .expect("fot quote case");
        let fot_pool = fot.pool.clone().into_pool_state();
        let fot_amount = fot.amount_in.parse::<u128>().unwrap();
        let clean_quote = quote_pool_exact_in(&fot_pool, fot.token_in, fot_amount).unwrap();
        let flagged_quote = quote_pool_exact_in_with_options(
            &fot_pool,
            fot.token_in,
            fot_amount,
            QuoteOptions { token_in_fee_on_transfer: true },
        )
        .unwrap();

        assert_eq!(flagged_quote.amount_out.to_string(), fot.expected.amount_out);
        assert_eq!(flagged_quote.amount_out, clean_quote.amount_out);
        assert!(flagged_quote.approximation);
        assert!(flagged_quote.confidence <= 0.5);
        assert_eq!(flagged_quote.confidence, fot.expected.confidence.unwrap());
        assert_eq!(fot.expected.approximation.as_deref(), Some("fot_unmodeled"));
        assert_eq!(flagged_quote.caveat, Some("fot_unmodeled"));
    }

    #[test]
    fn loads_ts_exporter_pool_baseline_fixture() {
        let pools = load_pool_baseline_from_path(load_pool_baseline_sample_fixture()).unwrap();
        assert_eq!(pools.len(), 2);

        let reserve = &pools[0];
        assert_eq!(reserve.protocol, Protocol::AerodromeVolatile);
        assert_eq!(
            reserve.token0,
            "0x4200000000000000000000000000000000000006".parse::<Address>().unwrap()
        );
        assert_eq!(reserve.decimals0, 18);
        assert_eq!(reserve.decimals1, 6);
        assert_eq!(reserve.fee, 5);
        assert_eq!(reserve.reserve0, 20_908_917_650_113_583_555u128);
        assert_eq!(reserve.reserve1, 32_916_653_549u128);
        assert_eq!(reserve.sqrt_price_x96, None);
        assert_eq!(reserve.liquidity, None);
        assert_eq!(reserve.tick, None);
        assert_eq!(reserve.tick_spacing, None);
        assert!(reserve.ticks.is_empty());

        let v3 = &pools[1];
        assert_eq!(v3.protocol, Protocol::UniswapV3);
        assert_eq!(v3.fee, 500);
        assert_eq!(v3.reserve0, 0);
        assert_eq!(v3.reserve1, 0);
        assert_eq!(v3.sqrt_price_x96, Some(1_771_595_571_142_957_112_070_506_167u128));
        assert_eq!(v3.liquidity, Some(123_456_789_012_345_678_901_234u128));
        assert_eq!(v3.tick, Some(-76012));
        assert_eq!(v3.tick_spacing, Some(10));
        assert_eq!(
            v3.ticks,
            vec![
                V3Tick { tick: -76020, liquidity_net: 123_456_789_012_345_678i128 },
                V3Tick { tick: -76010, liquidity_net: -123_456_789_012_345_678i128 },
            ]
        );
    }

    #[test]
    fn canonicalization_orientation_is_stable() {
        let p1 = PoolState::v2_like(
            addr(0xb1),
            Protocol::UniswapV2,
            addr(0xa1),
            addr(0xa2),
            1,
            1_000,
            1_100,
        );
        let p2 = PoolState::v2_like(
            addr(0xb2),
            Protocol::UniswapV2,
            addr(0xa1),
            addr(0xa2),
            1,
            1_100,
            1_000,
        );
        let c1 = find_negative_cycle_candidates(&[p1.clone(), p2.clone()], 10u128.pow(18));
        let c2 = find_negative_cycle_candidates(&[p2, p1], 10u128.pow(18));
        assert!(!c1.is_empty());
        assert_eq!(c1[0].fingerprint, c2[0].fingerprint);
        assert_eq!(c1[0].tokens[0], addr(0xa1));
    }

    #[test]
    fn candidate_fingerprint_is_hash_while_id_remains_human_edge_string() {
        let p1 = PoolState::v2_like(
            addr(0xb1),
            Protocol::UniswapV2,
            addr(0xa1),
            addr(0xa2),
            1,
            1_000,
            1_100,
        );
        let p2 = PoolState::v2_like(
            addr(0xb2),
            Protocol::UniswapV2,
            addr(0xa1),
            addr(0xa2),
            1,
            1_100,
            1_000,
        );

        let first = find_negative_cycle_candidates(&[p1.clone(), p2.clone()], 10u128.pow(18));
        let second = find_negative_cycle_candidates(&[p1, p2], 10u128.pow(18));
        let first_candidate = first.first().expect("candidate");
        let second_candidate = second.first().expect("candidate");

        assert_eq!(first_candidate.fingerprint.len(), 64);
        assert!(first_candidate.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!first_candidate.fingerprint.chars().any(|c| c.is_ascii_uppercase()));
        assert!(first_candidate.candidate_id.contains("->"));
        assert!(first_candidate.candidate_id.contains(':'));
        assert!(first_candidate.candidate_id.contains('|'));
        assert_ne!(first_candidate.fingerprint, first_candidate.candidate_id);
        assert_eq!(first_candidate.fingerprint, second_candidate.fingerprint);
        assert_eq!(first_candidate.candidate_id, second_candidate.candidate_id);
    }

    #[test]
    fn dirty_pool_subset_uses_reverse_map() {
        let pools = vec![
            PoolState::v2_like(addr(1), Protocol::UniswapV2, addr(10), addr(11), 30, 1, 1),
            PoolState::v2_like(addr(2), Protocol::UniswapV2, addr(12), addr(13), 30, 1, 1),
        ];
        let reverse = candidate_index_reverse_map(&pools);
        let mut dirty = BTreeSet::new();
        dirty.insert(addr(2));
        let (subset, truncated) = dirty_pool_subset(&pools, &dirty, &reverse);
        assert!(!truncated);
        assert_eq!(subset.len(), 1);
        assert_eq!(subset[0].pool, addr(2));
    }

    #[test]
    fn run_frame_expands_dirty_pool_to_full_baseline_cycle() {
        let dirty_pool = addr(0xa1);
        let clean_pool = addr(0xb2);
        let pools = vec![
            PoolState::v2_like(dirty_pool, Protocol::UniswapV2, addr(1), addr(2), 1, 1_000, 1_100),
            PoolState::v2_like(clean_pool, Protocol::UniswapV2, addr(1), addr(2), 1, 1_100, 1_000),
        ];
        let mut dirty = BTreeSet::new();
        dirty.insert(dirty_pool);
        let frame = run_frame(&pools, &dirty, &DryRunConfig::default(), NoActionGuard);
        assert_eq!(frame.health, "ok");
        assert!(frame.candidates.iter().any(|candidate| {
            candidate.pools.contains(&dirty_pool) && candidate.pools.contains(&clean_pool)
        }));
    }

    #[test]
    fn run_frame_skips_unknown_dirty_pool_without_full_baseline_search() {
        let pools = vec![PoolState::v2_like(
            addr(0xa1),
            Protocol::UniswapV2,
            addr(1),
            addr(2),
            1,
            1_000,
            1_100,
        )];
        let mut dirty = BTreeSet::new();
        dirty.insert(addr(0xff));
        let frame = run_frame(&pools, &dirty, &DryRunConfig::default(), NoActionGuard);
        assert_eq!(frame.health, "skipped");
        assert!(frame.candidates.is_empty());
        assert_eq!(frame.caveat.as_deref(), Some("dirty-pool-not-in-baseline"));
    }
    #[test]
    fn frame_bounds_and_backpressure_truncate() {
        let pools = vec![
            PoolState::v2_like(
                addr(0xb1),
                Protocol::UniswapV2,
                addr(0xa1),
                addr(0xa2),
                1,
                1_000,
                1_100,
            ),
            PoolState::v2_like(
                addr(0xb2),
                Protocol::UniswapV2,
                addr(0xa1),
                addr(0xa2),
                1,
                1_100,
                1_000,
            ),
        ];
        let config = DryRunConfig {
            max_pools_per_frame: 1,
            max_candidates_per_frame: 1,
            time_budget: Duration::from_micros(1),
            amount_in_wei: 1_000,
        };
        let frame = run_frame(&pools, &BTreeSet::new(), &config, NoActionGuard);
        assert!(frame.truncated);
        assert_eq!(frame.health, "truncated");
    }

    fn pack_slot0_word(sqrt_price_x96: u128, tick: i32) -> U256 {
        let tick_u24 = if tick < 0 {
            u32::try_from((1i64 << 24) + i64::from(tick)).unwrap()
        } else {
            u32::try_from(tick).unwrap()
        };
        U256::from(sqrt_price_x96) | (U256::from(tick_u24) << 160usize)
    }

    fn pack_univ2_reserves(reserve0: u128, reserve1: u128) -> U256 {
        U256::from(reserve0) | (U256::from(reserve1) << 112usize)
    }

    #[test]
    fn overlay_decodes_v3_slot0_and_liquidity_words() {
        let sqrt_price_x96 = 169_982_099_328_384_520_004_752u128;
        let tick = -261_057;
        let liquidity = 12_345_678_901_234_567_890u128;
        let pool = PoolState::v3(addr(0xc0), addr(1), addr(2), 500, 1, 99, 12_345, Vec::new());
        let slots = BTreeMap::from([
            (slot_key(V3_SLOT0_SLOT), pack_slot0_word(sqrt_price_x96, tick)),
            (slot_key(V3_LIQUIDITY_SLOT), U256::from(liquidity)),
        ]);

        let delta = PoolStateDelta::from_slots(
            &pool,
            &slots,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );

        assert_eq!(delta.sqrt_price_x96, Some(sqrt_price_x96));
        assert_eq!(delta.tick, Some(tick));
        assert_eq!(delta.liquidity, Some(liquidity));
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));
    }

    #[test]
    fn overlay_decodes_univ2_packed_reserves() {
        let pool =
            PoolState::v2_like(addr(0xc1), Protocol::UniswapV2, addr(1), addr(2), 30, 1_000, 2_000);
        let slots =
            BTreeMap::from([(slot_key(UNIV2_RESERVES_SLOT), pack_univ2_reserves(3_000, 4_000))]);

        let delta = PoolStateDelta::from_slots(
            &pool,
            &slots,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );

        assert_eq!(delta.reserve0, Some(3_000));
        assert_eq!(delta.reserve1, Some(4_000));
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-applied"));
    }

    #[test]
    fn overlay_caveats_equal_live_reserve_reads_as_verified() {
        let pool =
            PoolState::v2_like(addr(0xc4), Protocol::UniswapV2, addr(1), addr(2), 30, 1_000, 2_000);
        let slots =
            BTreeMap::from([(slot_key(UNIV2_RESERVES_SLOT), pack_univ2_reserves(1_000, 2_000))]);

        let delta = PoolStateDelta::from_slots(
            &pool,
            &slots,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );

        assert!(!delta.has_state_update());
        assert!(delta.caveats.iter().any(|c| c == "live-overlay-verified"));
        assert!(!delta.caveats.iter().any(|c| c == "stale-baseline-fallback"));
    }

    #[test]
    fn overlay_fail_closes_aerodrome_volatile_partial_and_stable_layouts() {
        let volatile = PoolState::v2_like(
            addr(0xc2),
            Protocol::AerodromeVolatile,
            addr(1),
            addr(2),
            5,
            1_000,
            2_000,
        );
        let partial =
            BTreeMap::from([(slot_key(AERO_VOLATILE_RESERVE0_SLOT), U256::from(3_000u128))]);
        let partial_delta = PoolStateDelta::from_slots(
            &volatile,
            &partial,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );
        assert!(!partial_delta.has_state_update());
        assert!(partial_delta.caveats.iter().any(|c| c == "partial-live-overlay"));

        let stable = PoolState::v2_like(
            addr(0xc3),
            Protocol::AerodromeStable,
            addr(1),
            addr(2),
            5,
            1_000,
            2_000,
        );
        let stable_delta = PoolStateDelta::from_slots(
            &stable,
            &partial,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );
        assert!(!stable_delta.has_state_update());
        assert!(stable_delta.caveats.iter().any(|c| c == "overlay-unsupported-protocol"));
    }

    #[test]
    fn run_frame_applies_live_reserve_overlay_before_quote() {
        let dirty_pool = addr(0xa1);
        let clean_pool = addr(0xb2);
        let pools = vec![
            PoolState::v2_like(dirty_pool, Protocol::UniswapV2, addr(1), addr(2), 1, 1_000, 1_000),
            PoolState::v2_like(clean_pool, Protocol::UniswapV2, addr(1), addr(2), 1, 1_000, 1_000),
        ];
        let mut dirty = BTreeSet::new();
        dirty.insert(dirty_pool);
        let baseline = run_frame(&pools, &dirty, &DryRunConfig::default(), NoActionGuard);
        assert!(baseline.candidates.is_empty());

        let slots =
            BTreeMap::from([(slot_key(UNIV2_RESERVES_SLOT), pack_univ2_reserves(1_000, 1_200))]);
        let delta = PoolStateDelta::from_slots(
            &pools[0],
            &slots,
            PoolOverlaySource::SlotDiff,
            Some(10),
            Some(10),
        );
        let overlay = BTreeMap::from([(dirty_pool, delta)]);
        let frame = run_frame_with_overlay(
            &pools,
            &dirty,
            &overlay,
            &DryRunConfig::default(),
            NoActionGuard,
        );

        assert_eq!(frame.health, "ok");
        assert!(frame.caveat.as_deref().is_some_and(|c| c.contains("live-overlay-applied")));
        assert!(frame.candidates.iter().any(|candidate| {
            candidate.pools.contains(&dirty_pool) && candidate.pools.contains(&clean_pool)
        }));
    }

    #[test]
    fn run_frame_prioritizes_dirty_pools_under_frame_cap() {
        let clean_counterparty = addr(0xb1);
        let unrelated = addr(0xb2);
        let dirty_pool = addr(0xb3);
        let pools = vec![
            PoolState::v2_like(
                clean_counterparty,
                Protocol::UniswapV2,
                addr(1),
                addr(2),
                1,
                1_000,
                1_000,
            ),
            PoolState::v2_like(unrelated, Protocol::UniswapV2, addr(3), addr(4), 1, 1_000, 1_000),
            PoolState::v2_like(dirty_pool, Protocol::UniswapV2, addr(1), addr(2), 1, 1_000, 1_200),
        ];
        let config =
            DryRunConfig { max_pools_per_frame: 2, amount_in_wei: 10, ..DryRunConfig::default() };
        let mut dirty = BTreeSet::new();
        dirty.insert(dirty_pool);

        let frame = run_frame(&pools, &dirty, &config, NoActionGuard);

        assert!(frame.candidates.iter().any(|candidate| candidate.pools.contains(&dirty_pool)));
        assert!(
            !frame.caveat.as_deref().unwrap_or_default().contains("dirty-pool-outside-frame-cap")
        );
    }

    #[test]
    fn run_frame_caveats_dirty_pool_over_cap() {
        let pools = vec![
            PoolState::v2_like(addr(0xb1), Protocol::UniswapV2, addr(1), addr(2), 1, 1_000, 1_100),
            PoolState::v2_like(addr(0xb2), Protocol::UniswapV2, addr(1), addr(2), 1, 1_100, 1_000),
        ];
        let config =
            DryRunConfig { max_pools_per_frame: 1, amount_in_wei: 10, ..DryRunConfig::default() };
        let mut dirty = BTreeSet::new();
        dirty.insert(addr(0xb1));
        dirty.insert(addr(0xb2));

        let frame = run_frame(&pools, &dirty, &config, NoActionGuard);

        assert!(frame.truncated);
        assert!(
            frame.caveat.as_deref().is_some_and(|c| c.contains("dirty-pool-outside-frame-cap"))
        );
    }

    #[test]
    fn compose_caveats_preserves_overlay_and_quote_status() {
        let caveat = compose_caveat_parts(&[
            Some("live-overlay-applied;bounded-frame-truncated"),
            Some("v3-price-limit"),
        ]);

        assert_eq!(
            caveat.as_deref(),
            Some("live-overlay-applied;bounded-frame-truncated;v3-price-limit"),
        );
    }

    #[test]
    fn disabled_by_default_and_guard_are_static() {
        assert!(!enabled_from_value(None));
        assert!(!enabled_from_value(Some(std::ffi::OsStr::new("true"))));
        assert!(enabled_from_value(Some(std::ffi::OsStr::new("1"))));
        assert_eq!(NoActionGuard.mode(), "dry-run-only");
    }
    #[test]
    fn loads_operator_pool_baseline_json() {
        let path = std::env::temp_dir()
            .join(format!("arb-dryrun-pools-{}-baseline.json", std::process::id()));
        let json = format!(
            r#"[{{"pool":"{pool}","protocol":"uniswap_v2","token0":"{token0}","token1":"{token1}","decimals0":18,"decimals1":6,"fee":30,"reserve0":1000,"reserve1":2000,"sqrt_price_x96":null,"liquidity":null,"tick":null,"tick_spacing":null,"ticks":[]}}]"#,
            pool = addr(0x55),
            token0 = addr(0x11),
            token1 = addr(0x22),
        );
        std::fs::write(&path, json).unwrap();
        let pools = load_pool_baseline_from_path(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].protocol, Protocol::UniswapV2);
        assert_eq!(pools[0].decimals1, 6);
    }
}
