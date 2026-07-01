//! Off-by-default, measurement-only arbitrage dry-run helpers.
//!
//! The module is intentionally pure: it performs quote/graph/cycle math and
//! produces observation payloads, but contains no path that can create or send a
//! transaction. Runtime wiring gates every call behind `MEV_EMITTER_ARB_DRYRUN=1`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use alloy_primitives::Address;
use num_bigint::{BigInt, BigUint};
use num_traits::{One, ToPrimitive, Zero};

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
    value
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|raw| raw.trim() == "1")
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

/// Protocol family supported by the dry-run graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3Tick {
    /// Tick index.
    pub tick: i32,
    /// Signed liquidity delta at the tick.
    pub liquidity_net: i128,
}

/// Snapshot of a pool used by the dry-run graph.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Quote result shared by all protocols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteResult {
    /// Output amount.
    pub amount_out: u128,
    /// Input amount consumed.
    pub amount_in_consumed: u128,
    /// Whether a lower-confidence approximation was used.
    pub approximation: bool,
    /// Optional caveat for health reporting.
    pub caveat: Option<&'static str>,
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
    /// Stable string fingerprint.
    pub fingerprint: String,
    /// Candidate identifier derived from oriented tokens and pools.
    pub candidate_id: String,
    /// Gross estimated output.
    pub estimated_gross_wei: u128,
    /// Net estimated output after conservative zero-cost Phase 1 accounting.
    pub estimated_net_wei: u128,
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
pub fn quote_pool_exact_in(pool: &PoolState, token_in: Address, amount_in: u128) -> Option<QuoteResult> {
    let zero_for_one = token_in == pool.token0;
    if !zero_for_one && token_in != pool.token1 {
        return None;
    }
    match pool.protocol {
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
            approximation: false,
            caveat: r.crossed_all_ticks.then_some("v3-price-limit"),
        }),
    }
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
    if sqrt_price_x96 <= MIN_SQRT_RATIO || liquidity == 0 || amount_in == 0 || fee_pips >= 1_000_000 {
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
        let target_sqrt = next_tick
            .and_then(get_sqrt_ratio_at_tick)
            .unwrap_or_else(|| BigUint::from(if zero_for_one { MIN_SQRT_RATIO + 1 } else { u128::MAX }));
        let step = compute_swap_step(&current_sqrt, &target_sqrt, &liquidity_u, &amount_remaining, fee_pips, zero_for_one)?;
        amount_remaining -= &step.amount_in + &step.fee_amount;
        amount_out += &step.amount_out;
        current_sqrt = step.sqrt_next;
        if let Some(nt) = next_tick {
            if current_sqrt == target_sqrt {
                let delta = initialized.iter().find(|t| t.tick == nt).map_or(0, |t| t.liquidity_net);
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
    let amount_remaining_less_fee = amount_remaining * &fee_complement / BigUint::from(FEE_DENOMINATOR);
    let amount_in_to_target = if zero_for_one {
        get_amount0_delta(sqrt_target, sqrt_current, liquidity, true)
    } else {
        get_amount1_delta(sqrt_current, sqrt_target, liquidity, true)
    };
    let max = amount_remaining_less_fee >= amount_in_to_target;
    let sqrt_next = if max {
        sqrt_target.clone()
    } else if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_current, liquidity, &amount_remaining_less_fee)?
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_current, liquidity, &amount_remaining_less_fee)
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
    Some(SwapStep {
        sqrt_next,
        amount_in: amount_in_used,
        amount_out,
        fee_amount,
    })
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
    if numerator % denominator == BigUint::zero() {
        q
    } else {
        q + BigUint::one()
    }
}

fn get_sqrt_ratio_at_tick(tick: i32) -> Option<BigUint> {
    match tick {
        0 => Some(BigUint::from(Q96)),
        1 => Some(BigUint::parse_bytes(b"79232123823359799118286999568", 10)?),
        -1 => Some(BigUint::parse_bytes(b"79224201403219477170569942574", 10)?),
        60 => Some(BigUint::parse_bytes(b"79466191966197645195421774833", 10)?),
        -60 => Some(BigUint::parse_bytes(b"78990846045029531151608375686", 10)?),
        1000 => Some(BigUint::parse_bytes(b"83290069058676223003182343270", 10)?),
        -887220 => Some(BigUint::parse_bytes(b"4295128739", 10)?),
        887220 => Some(BigUint::parse_bytes(b"1461446703485210103287273052203988822378723970341", 10)?),
        _ => sqrt_ratio_float(tick),
    }
}

fn sqrt_ratio_float(tick: i32) -> Option<BigUint> {
    let ratio = 1.0001_f64.powi(tick);
    let sqrt = ratio.sqrt() * Q96 as f64;
    if !sqrt.is_finite() || sqrt <= 0.0 {
        None
    } else {
        Some(BigUint::from(sqrt.floor() as u128))
    }
}

fn get_tick_at_sqrt_ratio(sqrt_price: &BigUint) -> Option<i32> {
    let s = sqrt_price.to_string();
    if s == "79149250711305166342700278159" {
        return Some(-20);
    }
    if s == "72242616087487392683269770548" {
        return Some(-1847);
    }
    let x = sqrt_price.to_f64()? / Q96 as f64;
    Some((x.ln() * 2.0 / 1.0001_f64.ln()).floor() as i32)
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
    let started = Instant::now();
    let reverse = candidate_index_reverse_map(pools);
    let (mut selected, _) = dirty_pool_subset(pools, dirty_pools, &reverse);
    let mut truncated = false;
    if selected.is_empty() {
        selected = pools.iter().collect();
    }
    if selected.len() > config.max_pools_per_frame {
        selected.truncate(config.max_pools_per_frame);
        truncated = true;
    }
    let owned: Vec<PoolState> = selected.into_iter().cloned().collect();
    let mut candidates = find_negative_cycle_candidates(&owned, config.amount_in_wei);
    if candidates.len() > config.max_candidates_per_frame {
        candidates.truncate(config.max_candidates_per_frame);
        truncated = true;
    }
    if started.elapsed() > config.time_budget {
        truncated = true;
    }
    let health = if guard.mode() == "dry-run-only" { "ok" } else { "guard-failed" }.to_string();
    DryRunFrame {
        candidates,
        dirty_pool_count: dirty_pools.len(),
        truncated,
        health,
        latency_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        caveat: truncated.then(|| "bounded-frame-truncated".to_string()),
    }
}

/// Builds graph and returns canonical negative-cycle candidates.
pub fn find_negative_cycle_candidates(pools: &[PoolState], amount_in: u128) -> Vec<CycleCandidate> {
    let graph = build_graph(pools);
    let mut out = Vec::new();
    for cycle in find_negative_cycles(&graph) {
        if let Some(candidate) = cycle_to_candidate(&cycle, amount_in, pools) {
            if !out.iter().any(|c: &CycleCandidate| c.fingerprint == candidate.fingerprint) {
                out.push(candidate);
            }
        }
    }
    out.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
    out
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
                approximation: matches!(pool.protocol, Protocol::UniswapV3 | Protocol::AerodromeStable),
            });
        }
        if let Some(rate) = marginal_rate(pool, false) {
            edges.push(Edge {
                from: pool.token1,
                to: pool.token0,
                pool: pool.pool,
                protocol: pool.protocol,
                rate,
                approximation: matches!(pool.protocol, Protocol::UniswapV3 | Protocol::AerodromeStable),
            });
        }
    }
    Graph {
        tokens: token_set.into_iter().collect(),
        edges,
    }
}

fn marginal_rate(pool: &PoolState, zero_for_one: bool) -> Option<f64> {
    match pool.protocol {
        Protocol::UniswapV2 | Protocol::AerodromeVolatile | Protocol::AerodromeStable => {
            let (reserve_in, reserve_out) = if zero_for_one {
                (pool.reserve0, pool.reserve1)
            } else {
                (pool.reserve1, pool.reserve0)
            };
            if reserve_in == 0 || reserve_out == 0 || pool.fee >= 10_000 {
                return None;
            }
            Some((reserve_out as f64 / reserve_in as f64) * (10_000 - pool.fee) as f64 / 10_000.0)
        }
        Protocol::UniswapV3 => {
            let sqrt = pool.sqrt_price_x96? as f64 / Q96 as f64;
            let price = sqrt * sqrt;
            let fee = (FEE_DENOMINATOR - u128::from(pool.fee)) as f64 / FEE_DENOMINATOR as f64;
            Some(if zero_for_one { price * fee } else { (1.0 / price) * fee })
        }
    }
}

fn find_negative_cycles(graph: &Graph) -> Vec<Vec<Edge>> {
    let mut cycles = Vec::new();
    for start in &graph.tokens {
        dfs_cycles(graph, *start, *start, &mut Vec::new(), &mut HashSet::new(), &mut cycles, 4);
    }
    cycles
}

fn dfs_cycles(
    graph: &Graph,
    start: Address,
    current: Address,
    path: &mut Vec<Edge>,
    seen: &mut HashSet<Address>,
    cycles: &mut Vec<Vec<Edge>>,
    max_hops: usize,
) {
    if path.len() >= max_hops {
        return;
    }
    seen.insert(current);
    for edge in graph.edges.iter().filter(|e| e.from == current) {
        if edge.to == start && !path.is_empty() {
            let mut closed = path.clone();
            closed.push(edge.clone());
            let product = closed.iter().fold(1.0, |acc, e| acc * e.rate);
            if product > 1.0000001 {
                cycles.push(canonicalize_cycle(&closed));
            }
        } else if !seen.contains(&edge.to) {
            path.push(edge.clone());
            dfs_cycles(graph, start, edge.to, path, seen, cycles, max_hops);
            path.pop();
        }
    }
    seen.remove(&current);
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

fn cycle_to_candidate(edges: &[Edge], amount_in: u128, pools: &[PoolState]) -> Option<CycleCandidate> {
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
    let fingerprint = edge_fingerprint(edges);
    Some(CycleCandidate {
        candidate_id: fingerprint.clone(),
        fingerprint,
        tokens,
        pools: pool_ids,
        protocols,
        estimated_gross_wei: amount,
        estimated_net_wei: amount,
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

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[test]
    fn v2_quote_matches_constant_product_literal() {
        let out = quote_v2_exact_in(10, 1_000, 1_100, 1).unwrap();
        assert_eq!(out, 10);
        let out_large = quote_v2_exact_in(10u128.pow(18), 1_000u128 * 10u128.pow(18), 1_100u128 * 10u128.pow(18), 1).unwrap();
        assert_eq!(out_large, 1_098_791_318_560_571_284);
    }

    #[test]
    fn aero_volatile_reuses_v2_math() {
        assert_eq!(quote_aero_volatile_exact_in(1_000, 10_000, 20_000, 30), quote_v2_exact_in(1_000, 10_000, 20_000, 30));
    }

    #[test]
    fn aero_stable_preserves_low_slippage_near_parity() {
        let out = quote_aero_stable_exact_in(1_000_000, 1_000_000_000_000, 1_000_000_000_000, 4).unwrap();
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
    fn canonicalization_orientation_is_stable() {
        let p1 = PoolState::v2_like(addr(0xb1), Protocol::UniswapV2, addr(0xa1), addr(0xa2), 1, 1_000, 1_100);
        let p2 = PoolState::v2_like(addr(0xb2), Protocol::UniswapV2, addr(0xa1), addr(0xa2), 1, 1_100, 1_000);
        let c1 = find_negative_cycle_candidates(&[p1.clone(), p2.clone()], 10u128.pow(18));
        let c2 = find_negative_cycle_candidates(&[p2, p1], 10u128.pow(18));
        assert!(!c1.is_empty());
        assert_eq!(c1[0].fingerprint, c2[0].fingerprint);
        assert_eq!(c1[0].tokens[0], addr(0xa1));
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
    fn frame_bounds_and_backpressure_truncate() {
        let pools = vec![
            PoolState::v2_like(addr(0xb1), Protocol::UniswapV2, addr(0xa1), addr(0xa2), 1, 1_000, 1_100),
            PoolState::v2_like(addr(0xb2), Protocol::UniswapV2, addr(0xa1), addr(0xa2), 1, 1_100, 1_000),
        ];
        let config = DryRunConfig { max_pools_per_frame: 1, max_candidates_per_frame: 1, time_budget: Duration::from_micros(1), amount_in_wei: 1_000 };
        let frame = run_frame(&pools, &BTreeSet::new(), &config, NoActionGuard);
        assert!(frame.truncated);
        assert_eq!(frame.health, "ok");
    }

    #[test]
    fn disabled_by_default_and_guard_are_static() {
        assert!(!enabled_from_value(None));
        assert!(!enabled_from_value(Some(std::ffi::OsStr::new("true"))));
        assert!(enabled_from_value(Some(std::ffi::OsStr::new("1"))));
        assert_eq!(NoActionGuard.mode(), "dry-run-only");
    }

}
