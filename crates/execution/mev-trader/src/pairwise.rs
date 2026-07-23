//! Deterministic bounded pairwise opportunity search.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::{self, Write},
    sync::LazyLock,
    time::Instant,
};

use alloy_primitives::{
    Address, B256, Uint,
    aliases::{I256, I512, U256, U512, U1024},
};
use alloy_rpc_types_engine::PayloadId;

use crate::{
    CancellationProbe, CanonicalDigest, CanonicalEncoder, ExactProtocol, MAX_CANDIDATES, MAX_PAIRS,
    MAX_PLANS_PER_FRAME, MAX_POOLS, ProcessedFrame,
};

/// Canonical Base WETH address used by the pinned pairwise authority.
pub const WETH: Address = Address::new([
    0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x06,
]);
/// Immutable source commit for the borrowed d44 pairwise authority.
pub const PAIRWISE_SOURCE_COMMIT: &str = "d44b316266c4231e6b82f88b460efdb00d70428a";
/// Immutable tree for `bench/pairwise-rust/**` at the authority commit.
pub const PAIRWISE_SOURCE_TREE: &str = "bd5b337329d98965abd25af0a823f19eb12c1baa";
/// Corrected issue-76 quote golden blob.
pub const ISSUE76_QUOTE_BLOB: &str = "d55983dbc8d075c6ba8012d5e0b40501122147ee";
/// Corrected issue-76 candidate-provenance blob.
pub const ISSUE76_PROVENANCE_BLOB: &str = "a6016953641a5c1bba3fefb172cc155439cd0442";
/// Known-optimistic issue-76 engine quote.
pub const ISSUE76_ENGINE_QUOTE: u64 = 1_229_736;
/// Independently observed issue-76 EVM output.
pub const ISSUE76_OBSERVED_QUOTE: u64 = 1_216_314;
/// Exact issue-76 engine-minus-observed gap.
pub const ISSUE76_QUOTE_GAP: u64 = 13_422;
/// Maximum pools retained per activated WETH/token market.
pub const K16: usize = 16;
/// Maximum fully K16-occupied tokens under the 512-pool bound.
pub const MAX_ACTIVATED_TOKENS: usize = 32;
/// Millionths denominator used by the pinned quote authority.
pub const FEE_DENOMINATOR: u32 = 1_000_000;
/// Minimum supported Uniswap V3 tick.
pub const PAIRWISE_MIN_TICK: i32 = -887_272;
/// Maximum supported Uniswap V3 tick.
pub const PAIRWISE_MAX_TICK: i32 = 887_272;
/// Minimum `TickMath` square-root ratio.
pub const MIN_SQRT_RATIO: u64 = 4_295_128_739;
/// Maximum `TickMath` square-root ratio as a canonical decimal.
pub const MAX_SQRT_RATIO_DECIMAL: &str = "1461446703485210103287273052203988822378723970342";

/// Lazily parsed maximum `TickMath` square-root ratio.
pub static MAX_SQRT_RATIO: LazyLock<U256> = LazyLock::new(|| {
    U256::from_str_radix(MAX_SQRT_RATIO_DECIMAL, 10).expect("valid maximum square-root ratio")
});
/// Pinned Uniswap V3 `TickMath` multipliers.
pub static TICK_MULTIPLIERS: LazyLock<[U256; 20]> = LazyLock::new(|| {
    [
        "fffcb933bd6fad37aa2d162d1a594001",
        "fff97272373d413259a46990580e213a",
        "fff2e50f5f656932ef12357cf3c7fdcc",
        "ffe5caca7e10e4e61c3624eaa0941cd0",
        "ffcb9843d60f6159c9db58835c926644",
        "ff973b41fa98c081472e6896dfb254c0",
        "ff2ea16466c96a3843ec78b326b52861",
        "fe5dee046a99a2a811c461f1969c3053",
        "fcbe86c790a88aedcffc83b479aa3a4",
        "f987a7253ac413176f2b074cf7815e54",
        "f3392b0822b70005940c7a398e4b70f3",
        "e7159475a2c29b7443b29c7fa6e889d9",
        "d097f3bdfd2022b8845ad8f792aa5825",
        "a9f746462d870fdf8a65dc1f90e061e5",
        "70d869a156d2a1b890bb3df62baf32f7",
        "31be135f97d08fd981231505542fcfa6",
        "9aa508b5b7a84e1c677de54f3e99bc9",
        "5d6af8dedb81196699c329225ee604",
        "2216e584f5fa1ea926041bedfe98",
        "48a170391f7dc42444e8fa2",
    ]
    .map(|value| U256::from_str_radix(value, 16).expect("valid TickMath multiplier"))
});

/// Deterministic quote and pairwise failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairwiseError {
    /// Input state is invalid or non-canonical.
    Invalid(&'static str),
    /// A checked numeric operation overflowed.
    Overflow(&'static str),
    /// A bounded quote could not consume its exact input.
    Exhausted(&'static str),
    /// An approved count cap was exceeded without truncation.
    LimitExceeded,
    /// Cooperative cancellation won and all output was dropped.
    Cancelled,
}

impl fmt::Display for PairwiseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(detail) => write!(formatter, "invalid pairwise input: {detail}"),
            Self::Overflow(detail) => write!(formatter, "pairwise numeric overflow: {detail}"),
            Self::Exhausted(detail) => write!(formatter, "pairwise quote exhausted: {detail}"),
            Self::LimitExceeded => formatter.write_str("pairwise limit exceeded"),
            Self::Cancelled => formatter.write_str("pairwise work cancelled"),
        }
    }
}

impl std::error::Error for PairwiseError {}

/// One initialized V3 tick in an immutable prepared pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairwiseV3Tick {
    /// Aligned initialized tick.
    pub tick: i32,
    /// Signed liquidity delta applied while crossing upward.
    pub liquidity_net: I256,
}

/// Immutable quote state detached from every provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedPoolQuote {
    /// V2-style constant-product state, including Aerodrome volatile.
    ConstantProduct {
        /// Reserve for token zero.
        reserve0: U256,
        /// Reserve for token one.
        reserve1: U256,
    },
    /// Aerodrome stable state.
    Stable {
        /// Reserve for token zero.
        reserve0: U256,
        /// Reserve for token one.
        reserve1: U256,
    },
    /// Uniswap V3 concentrated-liquidity state.
    V3 {
        /// Current Q96 square-root price.
        sqrt_price_x96: U256,
        /// Current raw active liquidity.
        liquidity: U256,
        /// Current tick.
        tick: i32,
        /// Positive attested tick spacing.
        tick_spacing: i32,
        /// Complete sorted initialized ticks for the prepared interior.
        ticks: Vec<PairwiseV3Tick>,
    },
}

impl PreparedPoolQuote {
    /// Constructs immutable V2-style constant-product quote state.
    pub const fn constant_product(reserve0: U256, reserve1: U256) -> Self {
        Self::ConstantProduct { reserve0, reserve1 }
    }

    /// Constructs immutable Aerodrome stable quote state.
    pub const fn stable(reserve0: U256, reserve1: U256) -> Self {
        Self::Stable { reserve0, reserve1 }
    }

    /// Constructs immutable Uniswap V3 quote state.
    pub const fn v3(
        sqrt_price_x96: U256,
        liquidity: U256,
        tick: i32,
        tick_spacing: i32,
        ticks: Vec<PairwiseV3Tick>,
    ) -> Self {
        Self::V3 { sqrt_price_x96, liquidity, tick, tick_spacing, ticks }
    }
}

/// Complete immutable pool state consumed by K16 and pairwise quoting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPoolState {
    /// Direct pool contract.
    pub pool: Address,
    /// Exact supported protocol.
    pub protocol: ExactProtocol,
    /// Canonical token zero.
    pub token0: Address,
    /// Canonical token one.
    pub token1: Address,
    /// Token-zero decimals.
    pub decimals0: u8,
    /// Token-one decimals.
    pub decimals1: u8,
    /// Fee in millionths.
    pub fee_pips: u32,
    /// Immutable provider-free quote state.
    pub quote: PreparedPoolQuote,
}

impl PreparedPoolState {
    /// Validates protocol/state coherence and every numeric precondition.
    pub fn validate(&self) -> Result<(), PairwiseError> {
        if self.pool.is_zero()
            || self.token0.is_zero()
            || self.token1.is_zero()
            || self.token0 == self.token1
            || self.fee_pips > FEE_DENOMINATOR
        {
            return Err(PairwiseError::Invalid("non-canonical pool metadata"));
        }
        match (&self.protocol, &self.quote) {
            (
                ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile,
                PreparedPoolQuote::ConstantProduct { reserve0, reserve1 },
            )
            | (ExactProtocol::AerodromeStable, PreparedPoolQuote::Stable { reserve0, reserve1 }) => {
                PairwiseMath::require_positive(reserve0, "reserve0 must be positive")?;
                PairwiseMath::require_positive(reserve1, "reserve1 must be positive")
            }
            (
                ExactProtocol::UniswapV3,
                PreparedPoolQuote::V3 { sqrt_price_x96, liquidity, tick, tick_spacing, ticks },
            ) => PairwiseMath::validate_v3_state(
                sqrt_price_x96,
                liquidity,
                *tick,
                *tick_spacing,
                self.fee_pips,
                ticks,
            ),
            _ => Err(PairwiseError::Invalid("protocol and quote state mismatch")),
        }
    }

    /// Returns the other token only for an exact WETH/token market.
    pub fn other_weth_token(&self) -> Option<Address> {
        if self.token0 == WETH && self.token1 != WETH {
            Some(self.token1)
        } else if self.token1 == WETH && self.token0 != WETH {
            Some(self.token0)
        } else {
            None
        }
    }

    /// Returns the exact post-victim K16 rank weight.
    pub fn weth_side_weight(&self) -> Option<U256> {
        match &self.quote {
            PreparedPoolQuote::ConstantProduct { reserve0, reserve1 }
            | PreparedPoolQuote::Stable { reserve0, reserve1 } => {
                if self.token0 == WETH {
                    Some(*reserve0)
                } else if self.token1 == WETH {
                    Some(*reserve1)
                } else {
                    None
                }
            }
            PreparedPoolQuote::V3 { liquidity, .. } => {
                if self.token0 == WETH || self.token1 == WETH { Some(*liquidity) } else { None }
            }
        }
    }

    /// Quotes exact input from immutable prepared state with cooperative cancellation.
    pub fn quote_exact_in(
        &self,
        token_in: Address,
        amount_in: U256,
        cancellation: &CancellationProbe,
    ) -> Result<U256, PairwiseError> {
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PairwiseError::Cancelled);
        }
        PairwiseMath::require_positive(&amount_in, "amountIn must be positive")?;
        let zero_for_one = if token_in == self.token0 {
            true
        } else if token_in == self.token1 {
            false
        } else {
            return Err(PairwiseError::Invalid("tokenIn is not a pool token"));
        };
        match &self.quote {
            PreparedPoolQuote::ConstantProduct { reserve0, reserve1 } => {
                let (reserve_in, reserve_out) =
                    if zero_for_one { (reserve0, reserve1) } else { (reserve1, reserve0) };
                PairwiseMath::quote_v2(&amount_in, reserve_in, reserve_out, self.fee_pips)
            }
            PreparedPoolQuote::Stable { reserve0, reserve1 } => {
                let (reserve_in, reserve_out, decimals_in, decimals_out) = if zero_for_one {
                    (reserve0, reserve1, self.decimals0, self.decimals1)
                } else {
                    (reserve1, reserve0, self.decimals1, self.decimals0)
                };
                PairwiseMath::quote_stable(
                    &amount_in,
                    reserve_in,
                    reserve_out,
                    self.fee_pips,
                    decimals_in,
                    decimals_out,
                    cancellation,
                )
            }
            PreparedPoolQuote::V3 { sqrt_price_x96, liquidity, tick, tick_spacing: _, ticks } => {
                PairwiseMath::quote_v3_exact_in(&PreparedV3QuoteParams {
                    sqrt_price_x96: *sqrt_price_x96,
                    liquidity: *liquidity,
                    tick: *tick,
                    fee_pips: self.fee_pips,
                    zero_for_one,
                    amount_in,
                    ticks,
                    sqrt_price_limit_x96: None,
                    cancellation,
                })
                .map(|result| result.amount_out)
            }
        }
    }
}

/// Parameters for a provider-free exact-input V3 quote.
#[derive(Debug)]
pub struct PreparedV3QuoteParams<'a> {
    /// Starting Q96 square-root price.
    pub sqrt_price_x96: U256,
    /// Starting active liquidity.
    pub liquidity: U256,
    /// Starting tick.
    pub tick: i32,
    /// Fee in millionths.
    pub fee_pips: u32,
    /// Direction from token zero to token one when true.
    pub zero_for_one: bool,
    /// Exact input amount.
    pub amount_in: U256,
    /// Complete sorted initialized ticks.
    pub ticks: &'a [PairwiseV3Tick],
    /// Optional exact Q96 limit.
    pub sqrt_price_limit_x96: Option<U256>,
    /// Cooperative cancellation probe.
    pub cancellation: &'a CancellationProbe,
}

/// Deterministic V3 quote result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V3QuoteResult {
    /// Exact output amount.
    pub amount_out: U256,
    /// Exact consumed input amount.
    pub amount_in_consumed: U256,
    /// Final Q96 square-root price.
    pub sqrt_price_x96_after: U256,
    /// Final tick.
    pub tick_after: i32,
}

/// One bounded V3 swap-step result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStep {
    /// Resulting Q96 square-root price.
    pub sqrt_ratio_next_x96: U256,
    /// Input excluding fee.
    pub amount_in: U256,
    /// Output amount.
    pub amount_out: U256,
    /// Fee amount.
    pub fee_amount: U256,
}

/// Pinned d44 numeric quote operations.
#[derive(Debug, Default, Clone, Copy)]
pub struct PairwiseMath;

impl PairwiseMath {
    /// Adds two fixed-width unsigned integers with an explicit overflow label.
    pub fn checked_add<const BITS: usize, const LIMBS: usize>(
        left: Uint<BITS, LIMBS>,
        right: Uint<BITS, LIMBS>,
        detail: &'static str,
    ) -> Result<Uint<BITS, LIMBS>, PairwiseError> {
        left.checked_add(right).ok_or(PairwiseError::Overflow(detail))
    }

    /// Subtracts two fixed-width unsigned integers with an explicit overflow label.
    pub fn checked_sub<const BITS: usize, const LIMBS: usize>(
        left: Uint<BITS, LIMBS>,
        right: Uint<BITS, LIMBS>,
        detail: &'static str,
    ) -> Result<Uint<BITS, LIMBS>, PairwiseError> {
        left.checked_sub(right).ok_or(PairwiseError::Overflow(detail))
    }

    /// Multiplies two fixed-width unsigned integers with an explicit overflow label.
    pub fn checked_mul<const BITS: usize, const LIMBS: usize>(
        left: Uint<BITS, LIMBS>,
        right: Uint<BITS, LIMBS>,
        detail: &'static str,
    ) -> Result<Uint<BITS, LIMBS>, PairwiseError> {
        left.checked_mul(right).ok_or(PairwiseError::Overflow(detail))
    }

    /// Left shifts a fixed-width unsigned integer with an explicit overflow label.
    pub fn checked_shl<const BITS: usize, const LIMBS: usize>(
        value: Uint<BITS, LIMBS>,
        shift: usize,
        detail: &'static str,
    ) -> Result<Uint<BITS, LIMBS>, PairwiseError> {
        value.checked_shl(shift).ok_or(PairwiseError::Overflow(detail))
    }

    /// Narrows U512 to U256 without truncation.
    pub fn narrow_u512(value: U512, detail: &'static str) -> Result<U256, PairwiseError> {
        U256::checked_from_limbs_slice(value.as_limbs()).ok_or(PairwiseError::Overflow(detail))
    }

    /// Narrows U1024 to U256 without truncation.
    pub fn narrow_u1024(value: U1024, detail: &'static str) -> Result<U256, PairwiseError> {
        U256::checked_from_limbs_slice(value.as_limbs()).ok_or(PairwiseError::Overflow(detail))
    }

    /// Requires a positive fixed-width unsigned integer.
    pub fn require_positive<const BITS: usize, const LIMBS: usize>(
        value: &Uint<BITS, LIMBS>,
        detail: &'static str,
    ) -> Result<(), PairwiseError> {
        if value.is_zero() { Err(PairwiseError::Invalid(detail)) } else { Ok(()) }
    }

    /// Requires a value to fit a bit width.
    pub const fn require_uint(
        value: &U256,
        bits: usize,
        detail: &'static str,
    ) -> Result<(), PairwiseError> {
        if value.bit_len() > bits { Err(PairwiseError::Overflow(detail)) } else { Ok(()) }
    }

    /// Quotes the pinned V2/Aerodrome-volatile exact-input formula.
    pub fn quote_v2(
        amount_in: &U256,
        reserve_in: &U256,
        reserve_out: &U256,
        fee_pips: u32,
    ) -> Result<U256, PairwiseError> {
        Self::require_positive(amount_in, "amountIn must be positive")?;
        Self::require_positive(reserve_in, "reserveIn must be positive")?;
        Self::require_positive(reserve_out, "reserveOut must be positive")?;
        if fee_pips > FEE_DENOMINATOR {
            return Err(PairwiseError::Invalid("feePips exceeds denominator"));
        }
        let amount_with_fee = Self::checked_mul(
            U512::from(*amount_in),
            U512::from(FEE_DENOMINATOR - fee_pips),
            "V2 post-fee input",
        )?;
        let numerator =
            Self::checked_mul(amount_with_fee, U512::from(*reserve_out), "V2 numerator")?;
        let denominator = Self::checked_add(
            Self::checked_mul(
                U512::from(*reserve_in),
                U512::from(FEE_DENOMINATOR),
                "V2 reserve term",
            )?,
            amount_with_fee,
            "V2 denominator",
        )?;
        let result = Self::narrow_u512(numerator / denominator, "V2 output")?;
        Self::require_positive(&result, "quote output must be positive")?;
        Ok(result)
    }

    /// Computes a checked decimal power for stable-pool normalization.
    pub fn pow10(decimals: u8) -> Result<U1024, PairwiseError> {
        let mut value = U1024::ONE;
        for _ in 0..decimals {
            value = Self::checked_mul(value, U1024::from(10u8), "decimal scale")?;
        }
        Ok(value)
    }

    /// Computes the pinned Aerodrome stable invariant.
    pub fn stable_invariant(x: U1024, y: U1024) -> Result<U1024, PairwiseError> {
        let x2 = Self::checked_mul(x, x, "stable x squared")?;
        let x3 = Self::checked_mul(x2, x, "stable x cubed")?;
        let y2 = Self::checked_mul(y, y, "stable y squared")?;
        let y3 = Self::checked_mul(y2, y, "stable y cubed")?;
        Self::checked_add(
            Self::checked_mul(x3, y, "stable x cubed times y")?,
            Self::checked_mul(y3, x, "stable y cubed times x")?,
            "stable invariant",
        )
    }

    /// Quotes the pinned Aerodrome stable exact-input formula.
    pub fn quote_stable(
        amount_in: &U256,
        reserve_in: &U256,
        reserve_out: &U256,
        fee_pips: u32,
        decimals_in: u8,
        decimals_out: u8,
        cancellation: &CancellationProbe,
    ) -> Result<U256, PairwiseError> {
        Self::require_positive(amount_in, "amountIn must be positive")?;
        Self::require_positive(reserve_in, "reserveIn must be positive")?;
        Self::require_positive(reserve_out, "reserveOut must be positive")?;
        if fee_pips > FEE_DENOMINATOR {
            return Err(PairwiseError::Invalid("feePips exceeds denominator"));
        }
        let scale = Self::pow10(18)?;
        let unit_in = Self::pow10(decimals_in)?;
        let unit_out = Self::pow10(decimals_out)?;
        let amount_after_fee = Self::checked_mul(
            U1024::from(*amount_in),
            U1024::from(FEE_DENOMINATOR - fee_pips),
            "stable post-fee input",
        )? / U1024::from(FEE_DENOMINATOR);
        let x = Self::checked_mul(U1024::from(*reserve_in), scale, "stable reserveIn")? / unit_in;
        let y_reserve =
            Self::checked_mul(U1024::from(*reserve_out), scale, "stable reserveOut")? / unit_out;
        let amount_scaled =
            Self::checked_mul(amount_after_fee, scale, "stable amountIn")? / unit_in;
        let invariant = Self::stable_invariant(x, y_reserve)?;
        let x_new = Self::checked_add(x, amount_scaled, "stable new x")?;
        let x2 = Self::checked_mul(x_new, x_new, "stable new x squared")?;
        let x3 = Self::checked_mul(x2, x_new, "stable new x cubed")?;
        let mut y = y_reserve;
        for _ in 0..255 {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            let lhs = Self::stable_invariant(x_new, y)?;
            if lhs == invariant {
                break;
            }
            let y2 = Self::checked_mul(y, y, "stable Newton y squared")?;
            let derivative = Self::checked_add(
                x3,
                Self::checked_mul(
                    Self::checked_mul(U1024::from(3u8), y2, "stable derivative")?,
                    x_new,
                    "stable derivative",
                )?,
                "stable derivative",
            )?;
            if derivative.is_zero() {
                break;
            }
            let next = if lhs > invariant {
                let delta =
                    Self::checked_sub(lhs, invariant, "stable Newton difference")? / derivative;
                if delta >= y {
                    U1024::ONE
                } else {
                    Self::checked_sub(y, delta, "stable Newton y")?
                }
            } else {
                Self::checked_add(
                    y,
                    Self::checked_sub(invariant, lhs, "stable Newton difference")? / derivative,
                    "stable Newton y",
                )?
            };
            if next == y {
                break;
            }
            y = next;
        }
        let mut lo = U1024::ZERO;
        let mut hi = y_reserve;
        for _ in 0..256 {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            if lo >= hi {
                break;
            }
            let mid = Self::checked_add(
                Self::checked_add(lo, hi, "stable bisection midpoint")?,
                U1024::ONE,
                "stable bisection midpoint",
            )? >> 1usize;
            if Self::stable_invariant(x_new, mid)? <= invariant {
                lo = mid;
            } else {
                hi = Self::checked_sub(mid, U1024::ONE, "stable bisection y")?;
            }
        }
        if lo < hi {
            return Err(PairwiseError::Exhausted("stable bisection"));
        }
        let selected = if lo < y || Self::stable_invariant(x_new, y)? > invariant { lo } else { y };
        if selected > y_reserve {
            return Err(PairwiseError::Invalid("stable solution exceeds reserve"));
        }
        let out = Self::checked_mul(
            Self::checked_sub(y_reserve, selected, "stable output reserve")?,
            unit_out,
            "stable output",
        )? / scale;
        Self::require_positive(&out, "stable output must be positive")?;
        Self::narrow_u1024(out, "stable output")
    }

    /// Computes floor(a*b/denominator) without intermediate truncation.
    pub fn mul_div(a: &U256, b: &U256, denominator: &U256) -> Result<U256, PairwiseError> {
        Self::require_positive(denominator, "mulDiv denominator")?;
        Self::narrow_u512(a.widening_mul(*b) / U512::from(*denominator), "mulDiv result")
    }

    /// Computes ceil(a*b/denominator) without intermediate truncation.
    pub fn mul_div_rounding_up(
        a: &U256,
        b: &U256,
        denominator: &U256,
    ) -> Result<U256, PairwiseError> {
        Self::require_positive(denominator, "mulDiv denominator")?;
        let product: U512 = a.widening_mul(*b);
        let wide_denominator = U512::from(*denominator);
        let mut result = Self::narrow_u512(product / wide_denominator, "mulDiv result")?;
        if product % wide_denominator != U512::ZERO {
            result = Self::checked_add(result, U256::ONE, "mulDiv rounding")?;
        }
        Ok(result)
    }

    /// Returns the pinned Q96 square-root ratio for one V3 tick.
    pub fn get_sqrt_ratio_at_tick(tick: i32) -> Result<U256, PairwiseError> {
        if !(PAIRWISE_MIN_TICK..=PAIRWISE_MAX_TICK).contains(&tick) {
            return Err(PairwiseError::Invalid("tick outside TickMath range"));
        }
        let absolute = tick.unsigned_abs();
        let mut ratio = if absolute & 1 != 0 {
            TICK_MULTIPLIERS[0]
        } else {
            Self::checked_shl(U256::ONE, 128, "TickMath initial ratio")?
        };
        for (bit, multiplier) in TICK_MULTIPLIERS.iter().enumerate().skip(1) {
            if absolute & (1u32 << bit) != 0 {
                ratio = Self::narrow_u512(
                    ratio.widening_mul(*multiplier) >> 128usize,
                    "TickMath ratio",
                )?;
            }
        }
        if tick > 0 {
            ratio = U256::MAX / ratio;
        }
        let remainder_mask = (U256::ONE << 32usize) - U256::ONE;
        let base = ratio >> 32usize;
        let rounded = if ratio & remainder_mask == U256::ZERO { base } else { base + U256::ONE };
        Self::require_uint(&rounded, 160, "sqrt ratio")?;
        Ok(rounded)
    }

    /// Returns the pinned V3 tick for one Q96 square-root ratio.
    pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: &U256) -> Result<i32, PairwiseError> {
        Self::require_uint(sqrt_price_x96, 160, "sqrtPriceX96")?;
        if *sqrt_price_x96 < U256::from(MIN_SQRT_RATIO) || *sqrt_price_x96 >= *MAX_SQRT_RATIO {
            return Err(PairwiseError::Invalid("sqrt ratio outside TickMath range"));
        }
        let mut low = PAIRWISE_MIN_TICK;
        let mut high = PAIRWISE_MAX_TICK;
        while low <= high {
            let middle = low + (high - low) / 2;
            let ratio = Self::get_sqrt_ratio_at_tick(middle)?;
            if ratio <= *sqrt_price_x96 {
                low = middle + 1;
            } else {
                high = middle - 1;
            }
        }
        Ok(high)
    }

    /// Computes token-zero delta between two Q96 prices.
    pub fn amount0_delta(
        sqrt_a: &U256,
        sqrt_b: &U256,
        liquidity: &U256,
        round_up: bool,
    ) -> Result<U256, PairwiseError> {
        let (a, b) = if sqrt_a <= sqrt_b { (*sqrt_a, *sqrt_b) } else { (*sqrt_b, *sqrt_a) };
        Self::require_positive(&a, "sqrt ratio")?;
        let numerator1 = Self::checked_shl(*liquidity, 96, "V3 liquidity numerator")?;
        let numerator2 = b - a;
        if round_up {
            let first = Self::mul_div_rounding_up(&numerator1, &numerator2, &b)?;
            Self::mul_div_rounding_up(&first, &U256::ONE, &a)
        } else {
            Ok(Self::mul_div(&numerator1, &numerator2, &b)? / a)
        }
    }

    /// Computes token-one delta between two Q96 prices.
    pub fn amount1_delta(
        sqrt_a: &U256,
        sqrt_b: &U256,
        liquidity: &U256,
        round_up: bool,
    ) -> Result<U256, PairwiseError> {
        let (a, b) = if sqrt_a <= sqrt_b { (*sqrt_a, *sqrt_b) } else { (*sqrt_b, *sqrt_a) };
        let delta = b - a;
        let q96 = U256::ONE << 96usize;
        if round_up {
            Self::mul_div_rounding_up(liquidity, &delta, &q96)
        } else {
            Self::mul_div(liquidity, &delta, &q96)
        }
    }

    /// Computes the next Q96 price from exact input.
    pub fn next_sqrt_price_from_input(
        sqrt_price: &U256,
        liquidity: &U256,
        amount_in: &U256,
        zero_for_one: bool,
    ) -> Result<U256, PairwiseError> {
        Self::require_positive(sqrt_price, "sqrt price")?;
        Self::require_positive(liquidity, "liquidity")?;
        let next = if zero_for_one {
            if amount_in.is_zero() {
                *sqrt_price
            } else {
                let numerator1 = *liquidity << 96usize;
                let product: U512 = amount_in.widening_mul(*sqrt_price);
                let denominator = U512::from(numerator1) + product;
                let numerator = U512::from(numerator1) * U512::from(*sqrt_price);
                let mut quotient = Self::narrow_u512(numerator / denominator, "next sqrt price")?;
                if numerator % denominator != U512::ZERO {
                    quotient += U256::ONE;
                }
                quotient
            }
        } else {
            let quotient = if amount_in.bit_len() <= 160 {
                (*amount_in << 96usize) / *liquidity
            } else {
                Self::mul_div(amount_in, &(U256::ONE << 96usize), liquidity)?
            };
            Self::checked_add(*sqrt_price, quotient, "next sqrt price")?
        };
        Self::require_uint(&next, 160, "next sqrt price")?;
        Ok(next)
    }

    /// Computes one pinned V3 exact-input swap step.
    pub fn compute_swap_step(
        sqrt_current: &U256,
        sqrt_target: &U256,
        liquidity: &U256,
        amount_remaining: &U256,
        fee_pips: u32,
    ) -> Result<SwapStep, PairwiseError> {
        if fee_pips >= FEE_DENOMINATOR {
            return Err(PairwiseError::Invalid("V3 feePips"));
        }
        Self::require_uint(liquidity, 128, "liquidity")?;
        let zero_for_one = sqrt_current >= sqrt_target;
        let amount_less_fee = Self::mul_div(
            amount_remaining,
            &U256::from(FEE_DENOMINATOR - fee_pips),
            &U256::from(FEE_DENOMINATOR),
        )?;
        let target_amount_in = if zero_for_one {
            Self::amount0_delta(sqrt_target, sqrt_current, liquidity, true)?
        } else {
            Self::amount1_delta(sqrt_current, sqrt_target, liquidity, true)?
        };
        let next = if amount_less_fee >= target_amount_in {
            *sqrt_target
        } else {
            Self::next_sqrt_price_from_input(
                sqrt_current,
                liquidity,
                &amount_less_fee,
                zero_for_one,
            )?
        };
        let reached = next == *sqrt_target;
        let amount_in = if zero_for_one {
            if reached {
                target_amount_in
            } else {
                Self::amount0_delta(&next, sqrt_current, liquidity, true)?
            }
        } else if reached {
            target_amount_in
        } else {
            Self::amount1_delta(sqrt_current, &next, liquidity, true)?
        };
        let amount_out = if zero_for_one {
            Self::amount1_delta(&next, sqrt_current, liquidity, false)?
        } else {
            Self::amount0_delta(sqrt_current, &next, liquidity, false)?
        };
        let fee_amount = if !reached {
            if amount_in > *amount_remaining {
                return Err(PairwiseError::Invalid("V3 step consumed too much"));
            }
            *amount_remaining - amount_in
        } else {
            Self::mul_div_rounding_up(
                &amount_in,
                &U256::from(fee_pips),
                &U256::from(FEE_DENOMINATOR - fee_pips),
            )?
        };
        Ok(SwapStep { sqrt_ratio_next_x96: next, amount_in, amount_out, fee_amount })
    }

    /// Validates complete prepared V3 state without allocating.
    pub fn validate_v3_state(
        sqrt_price_x96: &U256,
        liquidity: &U256,
        tick: i32,
        tick_spacing: i32,
        fee_pips: u32,
        ticks: &[PairwiseV3Tick],
    ) -> Result<(), PairwiseError> {
        Self::require_uint(sqrt_price_x96, 160, "sqrtPriceX96")?;
        Self::require_uint(liquidity, 128, "liquidity")?;
        Self::require_positive(sqrt_price_x96, "sqrtPriceX96")?;
        Self::require_positive(liquidity, "liquidity")?;
        if !(PAIRWISE_MIN_TICK..=PAIRWISE_MAX_TICK).contains(&tick)
            || tick_spacing <= 0
            || fee_pips >= FEE_DENOMINATOR
        {
            return Err(PairwiseError::Invalid("V3 metadata"));
        }
        for (index, entry) in ticks.iter().enumerate() {
            if !(PAIRWISE_MIN_TICK..=PAIRWISE_MAX_TICK).contains(&entry.tick)
                || entry.tick.rem_euclid(tick_spacing) != 0
                || index > 0 && ticks[index - 1].tick >= entry.tick
            {
                return Err(PairwiseError::Invalid("initialized ticks"));
            }
        }
        Ok(())
    }

    /// Applies one signed V3 liquidity delta without truncation.
    pub fn add_liquidity_delta(liquidity: &U256, delta: &I256) -> Result<U256, PairwiseError> {
        let magnitude = U256::checked_from_limbs_slice(delta.unsigned_abs().as_limbs())
            .ok_or(PairwiseError::Overflow("liquidity delta"))?;
        let value = if delta.is_negative() {
            liquidity.checked_sub(magnitude).ok_or(PairwiseError::Invalid("liquidity underflow"))?
        } else {
            liquidity.checked_add(magnitude).ok_or(PairwiseError::Overflow("liquidity"))?
        };
        Self::require_uint(&value, 128, "liquidity")?;
        Ok(value)
    }

    /// Quotes exact-input V3 against immutable prepared ticks.
    pub fn quote_v3_exact_in(
        params: &PreparedV3QuoteParams<'_>,
    ) -> Result<V3QuoteResult, PairwiseError> {
        Self::require_positive(&params.amount_in, "amountIn")?;
        let limit = params.sqrt_price_limit_x96.unwrap_or_else(|| {
            if params.zero_for_one {
                U256::from(MIN_SQRT_RATIO + 1)
            } else {
                *MAX_SQRT_RATIO - U256::ONE
            }
        });
        let valid_limit = if params.zero_for_one {
            limit < params.sqrt_price_x96 && limit >= U256::from(MIN_SQRT_RATIO)
        } else {
            limit > params.sqrt_price_x96 && limit <= *MAX_SQRT_RATIO
        };
        if !valid_limit {
            return Err(PairwiseError::Invalid("sqrt price limit"));
        }
        let mut sqrt_price = params.sqrt_price_x96;
        let mut liquidity = params.liquidity;
        let mut tick = params.tick;
        let mut remaining = params.amount_in;
        let mut output = U256::ZERO;
        while !remaining.is_zero() && sqrt_price != limit {
            if !params.cancellation.checkpoint(Instant::now(), true) {
                params.cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            let start = sqrt_price;
            let next = if params.zero_for_one {
                params.ticks.iter().rev().find(|entry| entry.tick <= tick)
            } else {
                params.ticks.iter().find(|entry| entry.tick > tick)
            };
            let Some(next_tick) = next else {
                let step = Self::compute_swap_step(
                    &sqrt_price,
                    &limit,
                    &liquidity,
                    &remaining,
                    params.fee_pips,
                )?;
                let consumed =
                    Self::checked_add(step.amount_in, step.fee_amount, "V3 consumed input")?;
                if consumed.is_zero() || consumed > remaining {
                    return Err(PairwiseError::Exhausted("V3 progress"));
                }
                remaining -= consumed;
                output = Self::checked_add(output, step.amount_out, "V3 output")?;
                sqrt_price = step.sqrt_ratio_next_x96;
                if !remaining.is_zero() {
                    return Err(PairwiseError::Exhausted("V3 initialized ticks"));
                }
                tick = Self::get_tick_at_sqrt_ratio(&sqrt_price)?;
                break;
            };
            let next_price = Self::get_sqrt_ratio_at_tick(next_tick.tick)?;
            let reaches_limit =
                if params.zero_for_one { next_price < limit } else { next_price > limit };
            let target = if reaches_limit { limit } else { next_price };
            let step = Self::compute_swap_step(
                &sqrt_price,
                &target,
                &liquidity,
                &remaining,
                params.fee_pips,
            )?;
            let consumed = Self::checked_add(step.amount_in, step.fee_amount, "V3 consumed input")?;
            if consumed > remaining {
                return Err(PairwiseError::Exhausted("V3 progress"));
            }
            remaining -= consumed;
            output = Self::checked_add(output, step.amount_out, "V3 output")?;
            sqrt_price = step.sqrt_ratio_next_x96;
            if sqrt_price == next_price && !reaches_limit {
                let delta = if params.zero_for_one {
                    next_tick
                        .liquidity_net
                        .checked_neg()
                        .ok_or(PairwiseError::Overflow("liquidity delta"))?
                } else {
                    next_tick.liquidity_net
                };
                liquidity = Self::add_liquidity_delta(&liquidity, &delta)?;
                tick = if params.zero_for_one { next_tick.tick - 1 } else { next_tick.tick };
            } else if sqrt_price != start {
                tick = Self::get_tick_at_sqrt_ratio(&sqrt_price)?;
            }
        }
        if !remaining.is_zero() {
            return Err(PairwiseError::Exhausted("V3 price limit"));
        }
        Self::require_positive(&output, "V3 output")?;
        Ok(V3QuoteResult {
            amount_out: output,
            amount_in_consumed: params.amount_in,
            sqrt_price_x96_after: sqrt_price,
            tick_after: tick,
        })
    }
}

/// Frozen d44 optimizer bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeBounds {
    /// Inclusive minimum input.
    pub lo: U256,
    /// Inclusive maximum input.
    pub hi: U256,
    /// Coarse linear grid points.
    pub grid_points: usize,
    /// Highest-ranked basins refined.
    pub top_k: usize,
    /// Golden-search integer tolerance.
    pub tolerance: U256,
    /// Optional pinned dense-grid subdivision count.
    pub densify_points: Option<usize>,
}

impl Default for SizeBounds {
    fn default() -> Self {
        Self {
            lo: U256::from(1_000_000_000_000u64),
            hi: U256::from(1_000_000_000_000_000_000u64),
            grid_points: 11,
            top_k: 3,
            tolerance: U256::from(1_000_000_000u64),
            densify_points: None,
        }
    }
}

/// One optimized exact-input result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizedSize {
    /// Selected exact input.
    pub amount: U256,
    /// Signed gross output-minus-input.
    pub profit: I512,
}

/// Indexed optimizer sample used for deterministic basin ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizerSample {
    /// Position in the sampled grid.
    pub index: usize,
    /// Exact evaluated result.
    pub result: OptimizedSize,
}

/// Cached deterministic quote evaluator used by the pinned optimizer.
#[derive(Debug)]
pub struct CachedEvaluator<Evaluate> {
    evaluate: Evaluate,
    values: HashMap<U256, U256>,
}

impl<Evaluate> CachedEvaluator<Evaluate>
where
    Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
{
    /// Creates an empty evaluator cache.
    pub fn new(evaluate: Evaluate) -> Self {
        Self { evaluate, values: HashMap::new() }
    }

    /// Returns a cached exact output.
    pub fn output(&mut self, amount: &U256) -> Result<U256, PairwiseError> {
        if let Some(output) = self.values.get(amount) {
            return Ok(*output);
        }
        let output = (self.evaluate)(amount)?;
        self.values.insert(*amount, output);
        Ok(output)
    }

    /// Returns a signed output-minus-input result.
    pub fn result(&mut self, amount: &U256) -> Result<OptimizedSize, PairwiseError> {
        let output = I512::from_raw(U512::from(self.output(amount)?));
        let input = I512::from_raw(U512::from(*amount));
        let profit =
            output.checked_sub(input).ok_or(PairwiseError::Overflow("optimizer profit"))?;
        Ok(OptimizedSize { amount: *amount, profit })
    }
}

/// Pinned d44 optimizer implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct PairwiseOptimizer;

impl PairwiseOptimizer {
    /// Chooses the better result, breaking ties by smaller input.
    pub fn better(left: OptimizedSize, right: OptimizedSize) -> OptimizedSize {
        if right.profit > left.profit || right.profit == left.profit && right.amount < left.amount {
            right
        } else {
            left
        }
    }

    /// Sorts results by descending profit then ascending amount.
    pub fn sort_best_first(values: &mut [OptimizedSize]) {
        values.sort_by(|left, right| {
            right.profit.cmp(&left.profit).then_with(|| left.amount.cmp(&right.amount))
        });
    }

    /// Builds the pinned checked linear grid.
    pub fn build_grid(bounds: &SizeBounds) -> Result<Vec<U256>, PairwiseError> {
        if bounds.hi < bounds.lo {
            return Err(PairwiseError::Invalid("optimizer bounds"));
        }
        if bounds.lo == bounds.hi {
            return Ok(vec![bounds.lo]);
        }
        let points = bounds.grid_points.clamp(8, 12);
        let denominator = U256::from(points - 1);
        let span = bounds.hi - bounds.lo;
        let mut seen = BTreeSet::new();
        let mut grid = Vec::with_capacity(points);
        for index in 0..points {
            let offset = span
                .checked_mul(U256::from(index))
                .ok_or(PairwiseError::Overflow("optimizer grid"))?
                / denominator;
            let amount =
                bounds.lo.checked_add(offset).ok_or(PairwiseError::Overflow("optimizer grid"))?;
            if seen.insert(amount) {
                grid.push(amount);
            }
        }
        Ok(grid)
    }

    /// Evaluates one amount grid while preserving each original index.
    pub fn samples<Evaluate>(
        amounts: &[U256],
        evaluator: &mut CachedEvaluator<Evaluate>,
        cancellation: &CancellationProbe,
    ) -> Result<Vec<OptimizerSample>, PairwiseError>
    where
        Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
    {
        let mut samples = Vec::with_capacity(amounts.len());
        for (index, amount) in amounts.iter().enumerate() {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            samples.push(OptimizerSample { index, result: evaluator.result(amount)? });
        }
        Ok(samples)
    }

    /// Returns every non-strict local maximum in original grid order.
    pub fn local_maxima(values: &[OptimizerSample]) -> Vec<OptimizerSample> {
        values
            .iter()
            .enumerate()
            .filter(|(index, point)| {
                let previous = index.checked_sub(1).and_then(|at| values.get(at));
                let next = values.get(index + 1);
                previous.is_none_or(|value| point.result.profit >= value.result.profit)
                    && next.is_none_or(|value| point.result.profit >= value.result.profit)
            })
            .map(|(_, value)| *value)
            .collect()
    }

    /// Ranks basins by descending profit then ascending amount.
    pub fn rank_basins(mut values: Vec<OptimizerSample>, top_k: usize) -> Vec<OptimizerSample> {
        values.sort_by(|left, right| {
            right
                .result
                .profit
                .cmp(&left.result.profit)
                .then_with(|| left.result.amount.cmp(&right.result.amount))
        });
        values.truncate(top_k);
        values
    }

    /// Builds the pinned dense grid between every coarse sample.
    pub fn build_dense_grid(
        grid: &[U256],
        sub_points: usize,
        cancellation: &CancellationProbe,
    ) -> Result<Vec<U256>, PairwiseError> {
        let mut amounts = BTreeSet::new();
        for (index, amount) in grid.iter().enumerate() {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            amounts.insert(*amount);
            let Some(next) = grid.get(index + 1) else { continue };
            let span = next.checked_sub(*amount).ok_or(PairwiseError::Overflow("dense span"))?;
            if span <= U256::ONE {
                continue;
            }
            let steps = U256::from(sub_points + 1);
            for sub_index in 1..=sub_points {
                if !cancellation.checkpoint(Instant::now(), true) {
                    cancellation.acknowledge_drop();
                    return Err(PairwiseError::Cancelled);
                }
                let offset = span
                    .checked_mul(U256::from(sub_index))
                    .ok_or(PairwiseError::Overflow("dense offset"))?
                    / steps;
                amounts.insert(
                    amount.checked_add(offset).ok_or(PairwiseError::Overflow("dense amount"))?,
                );
            }
        }
        Ok(amounts.into_iter().collect())
    }

    /// Scans a small integer bracket exactly.
    pub fn scan_integer_bracket<Evaluate>(
        lo: U256,
        hi: U256,
        evaluator: &mut CachedEvaluator<Evaluate>,
        mut best: OptimizedSize,
        cancellation: &CancellationProbe,
    ) -> Result<OptimizedSize, PairwiseError>
    where
        Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
    {
        let mut current = lo;
        while current <= hi {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            best = Self::better(best, evaluator.result(&current)?);
            if current == hi {
                break;
            }
            current =
                current.checked_add(U256::ONE).ok_or(PairwiseError::Overflow("bracket scan"))?;
        }
        Ok(best)
    }

    /// Refines one sampled basin with the pinned integer golden-section search.
    pub fn golden_section_search<Evaluate>(
        mut lo: U256,
        mut hi: U256,
        evaluator: &mut CachedEvaluator<Evaluate>,
        tolerance: U256,
        cancellation: &CancellationProbe,
    ) -> Result<OptimizedSize, PairwiseError>
    where
        Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
    {
        if hi < lo {
            return Err(PairwiseError::Invalid("golden-section bounds"));
        }
        let tolerance = tolerance.max(U256::ONE);
        let mut best = Self::better(evaluator.result(&lo)?, evaluator.result(&hi)?);
        while hi.checked_sub(lo).ok_or(PairwiseError::Overflow("golden span"))? > tolerance {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            let span = hi - lo;
            let mut left = lo
                .checked_add(
                    span.checked_mul(U256::from(382u16))
                        .ok_or(PairwiseError::Overflow("golden left"))?
                        / U256::from(1000u16),
                )
                .ok_or(PairwiseError::Overflow("golden left"))?;
            let mut right = lo
                .checked_add(
                    span.checked_mul(U256::from(618u16))
                        .ok_or(PairwiseError::Overflow("golden right"))?
                        / U256::from(1000u16),
                )
                .ok_or(PairwiseError::Overflow("golden right"))?;
            if left <= lo {
                left = lo.checked_add(U256::ONE).ok_or(PairwiseError::Overflow("golden left"))?;
            }
            if right >= hi {
                right = hi.checked_sub(U256::ONE).ok_or(PairwiseError::Overflow("golden right"))?;
            }
            if left >= right {
                return Self::scan_integer_bracket(lo, hi, evaluator, best, cancellation);
            }
            let left_result = evaluator.result(&left)?;
            let right_result = evaluator.result(&right)?;
            best = Self::better(best, Self::better(left_result, right_result));
            if left_result.profit < right_result.profit {
                lo = left;
            } else {
                hi = right;
            }
        }
        if hi - lo <= U256::from(32u8) {
            Self::scan_integer_bracket(lo, hi, evaluator, best, cancellation)
        } else {
            Ok(best)
        }
    }

    /// Runs the pinned integer hill climb from one dense-grid basin.
    pub fn hill_climb<Evaluate>(
        start: U256,
        lo: U256,
        hi: U256,
        evaluator: &mut CachedEvaluator<Evaluate>,
        cancellation: &CancellationProbe,
    ) -> Result<OptimizedSize, PairwiseError>
    where
        Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
    {
        let clamp = |amount: U256| {
            if amount < lo {
                lo
            } else if amount > hi {
                hi
            } else {
                amount
            }
        };
        let mut current = evaluator.result(&clamp(start))?;
        let mut step = if lo == hi { U256::ONE } else { hi - lo };
        while !step.is_zero() {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            let lower = if current.amount >= step { current.amount - step } else { U256::ZERO };
            let upper =
                current.amount.checked_add(step).ok_or(PairwiseError::Overflow("hill upper"))?;
            let mut moved = false;
            for candidate in [clamp(upper), clamp(lower)] {
                if candidate == current.amount {
                    continue;
                }
                let result = evaluator.result(&candidate)?;
                if result.profit > current.profit {
                    current = result;
                    moved = true;
                    break;
                }
            }
            if !moved {
                step >>= 1usize;
            }
        }
        Ok(current)
    }

    /// Runs the complete frozen d44 grid, dense-grid, golden, and hill searches.
    pub fn optimize<Evaluate>(
        evaluate: Evaluate,
        bounds: &SizeBounds,
        cancellation: &CancellationProbe,
    ) -> Result<OptimizedSize, PairwiseError>
    where
        Evaluate: FnMut(&U256) -> Result<U256, PairwiseError>,
    {
        if bounds.hi < bounds.lo {
            return Err(PairwiseError::Invalid("optimizer bounds"));
        }
        let grid = Self::build_grid(bounds)?;
        let top_k = bounds.top_k.max(1);
        let mut evaluator = CachedEvaluator::new(evaluate);
        let coarse = Self::samples(&grid, &mut evaluator, cancellation)?;
        let mut collected = coarse.iter().map(|value| value.result).collect::<Vec<_>>();
        let maxima = Self::local_maxima(&coarse);
        let coarse_candidates =
            Self::rank_basins(if maxima.is_empty() { coarse } else { maxima }, top_k);
        for basin in coarse_candidates {
            let bracket_lo = if basin.index > 0 { grid[basin.index - 1] } else { bounds.lo };
            let bracket_hi =
                if basin.index + 1 < grid.len() { grid[basin.index + 1] } else { bounds.hi };
            collected.push(Self::golden_section_search(
                bracket_lo,
                bracket_hi,
                &mut evaluator,
                bounds.tolerance,
                cancellation,
            )?);
        }

        let sub_points = bounds.densify_points.unwrap_or(17).clamp(4, 33);
        let dense_grid = Self::build_dense_grid(&grid, sub_points, cancellation)?;
        let dense = Self::samples(&dense_grid, &mut evaluator, cancellation)?;
        let dense_basins = Self::rank_basins(Self::local_maxima(&dense), top_k);
        for basin in dense_basins {
            let bracket_lo =
                if basin.index > 0 { dense[basin.index - 1].result.amount } else { bounds.lo };
            let bracket_hi = if basin.index + 1 < dense.len() {
                dense[basin.index + 1].result.amount
            } else {
                bounds.hi
            };
            collected.push(Self::golden_section_search(
                bracket_lo,
                bracket_hi,
                &mut evaluator,
                bounds.tolerance,
                cancellation,
            )?);
            collected.push(Self::hill_climb(
                basin.result.amount,
                bracket_lo,
                bracket_hi,
                &mut evaluator,
                cancellation,
            )?);
        }
        Self::sort_best_first(&mut collected);
        collected.into_iter().next().ok_or(PairwiseError::Invalid("empty candidates"))
    }
}

/// One K16-ranked activated WETH/token market borrowing immutable prepared state.
#[derive(Debug)]
pub struct RankedMarket<'a> {
    /// Non-WETH market token.
    pub token: Address,
    /// Pools ordered by descending WETH-side weight then address bytes.
    pub pools: Vec<&'a PreparedPoolState>,
}

/// One fixed two-hop pairwise route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackrunHop {
    /// Direct pool contract.
    pub pool: Address,
    /// Exact protocol.
    pub protocol: ExactProtocol,
    /// Exact input token.
    pub token_in: Address,
    /// Exact output token.
    pub token_out: Address,
    /// Canonical sizing fee in pips (denominator `1e6`) carried straight from the
    /// hash-pinned [`PreparedPoolState`] the sizing engine read (R8 fee-SOURCE).
    /// It feeds the plan digest (integrity), NOT the d44 candidate wire, and is the
    /// SOLE fee the submitter converts to the executor ABI `feeBps` — the submitter
    /// never re-derives or accepts a caller-trusted fee.
    pub fee_pips: u32,
}

/// One internal d44-compatible pairwise candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairwiseCandidate {
    /// Fixture identity used only by canonical parity bytes.
    pub fixture_id: String,
    /// Canonical directed pair identity.
    pub directed_key: String,
    /// Fixed two-hop closed route.
    pub route: [BackrunHop; 2],
    /// Optimized exact input.
    pub amount_in: U256,
    /// Exact output.
    pub amount_out: U256,
    /// Signed gross output-minus-input.
    pub gross_profit: I512,
}

/// Frame identity attached to a measurement-only plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementContext {
    /// Hash-pinned parent state.
    pub parent_hash: B256,
    /// Same-block number.
    pub block_number: u64,
    /// Exact predecessor flashblock index.
    pub predecessor_index: u64,
    /// Exact eight-byte generation payload identifier.
    pub payload_id: PayloadId,
    /// Bound victim transaction hash.
    pub victim: B256,
}

/// Self-excluding digest of canonical measurement bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackrunPlanDigest(pub B256);

/// Public fixed two-hop measurement DTO with no transaction-adjacent conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackrunPlan {
    /// Hash-pinned parent state.
    pub parent_hash: B256,
    /// Same-block number.
    pub block_number: u64,
    /// Exact predecessor flashblock index.
    pub predecessor_index: u64,
    /// Exact generation payload identifier.
    pub payload_id: PayloadId,
    /// Bound victim transaction hash.
    pub victim: B256,
    /// Fixed two-hop closed route.
    pub route: [BackrunHop; 2],
    /// Exact input amount.
    pub amount_in: U256,
    /// Exact output amount.
    pub amount_out: U256,
    /// Positive gross output-minus-input.
    pub gross_profit: U256,
    /// Self-excluding canonical digest.
    pub digest: BackrunPlanDigest,
}

/// K16 ranking, pair enumeration, d44 quoting, and max-one measurement selection.
#[derive(Debug, Default, Clone, Copy)]
pub struct PairwiseEngine;

impl PairwiseEngine {
    /// Validates and ranks exact activated markets without mutating prepared state.
    pub fn rank_k16<'a>(
        pools: &'a [PreparedPoolState],
        dirty_pools: &[Address],
        cancellation: &CancellationProbe,
    ) -> Result<Vec<RankedMarket<'a>>, PairwiseError> {
        if pools.len() > MAX_POOLS || dirty_pools.len() > MAX_POOLS {
            return Err(PairwiseError::LimitExceeded);
        }
        let mut addresses = BTreeSet::new();
        for pool in pools {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            pool.validate()?;
            if !addresses.insert(pool.pool) {
                return Err(PairwiseError::Invalid("duplicate pool"));
            }
        }
        let dirty: BTreeSet<_> = dirty_pools.iter().copied().collect();
        if dirty.len() != dirty_pools.len() || !dirty.is_subset(&addresses) {
            return Err(PairwiseError::Invalid("dirty pool set"));
        }
        let mut activated = BTreeSet::new();
        for pool in pools {
            if dirty.contains(&pool.pool)
                && let Some(token) = pool.other_weth_token()
            {
                activated.insert(token);
            }
        }
        let mut by_token = BTreeMap::<Address, Vec<&PreparedPoolState>>::new();
        for pool in pools {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            let Some(token) = pool.other_weth_token() else { continue };
            if activated.contains(&token) {
                by_token.entry(token).or_default().push(pool);
            }
        }
        let mut markets = Vec::with_capacity(by_token.len());
        for (token, mut market_pools) in by_token {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PairwiseError::Cancelled);
            }
            market_pools.sort_by(|left, right| {
                right
                    .weth_side_weight()
                    .cmp(&left.weth_side_weight())
                    .then_with(|| left.pool.cmp(&right.pool))
            });
            market_pools.truncate(K16);
            markets.push(RankedMarket { token, pools: market_pools });
        }
        Ok(markets)
    }

    /// Counts all post-K16 directed pairs with checked arithmetic and no allocation.
    pub fn pair_count(markets: &[RankedMarket<'_>]) -> Result<usize, PairwiseError> {
        let mut count = 0usize;
        for market in markets {
            let market_count = market
                .pools
                .len()
                .checked_mul(market.pools.len().saturating_sub(1))
                .ok_or(PairwiseError::LimitExceeded)?;
            count = count.checked_add(market_count).ok_or(PairwiseError::LimitExceeded)?;
        }
        if count > MAX_PAIRS { Err(PairwiseError::LimitExceeded) } else { Ok(count) }
    }

    /// Returns the canonical lowercase d44 protocol label.
    pub const fn protocol_label(protocol: ExactProtocol) -> &'static str {
        match protocol {
            ExactProtocol::UniswapV2 => "uniswap_v2",
            ExactProtocol::AerodromeVolatile => "aerodrome_volatile",
            ExactProtocol::AerodromeStable => "aerodrome_stable",
            ExactProtocol::UniswapV3 => "uniswap_v3",
        }
    }

    /// Returns one canonical d44 directed pair key.
    pub fn directed_key(
        first: &PreparedPoolState,
        second: &PreparedPoolState,
        token: Address,
    ) -> String {
        format!(
            "{:#x}:{:#x}:{:#x}:{}>{:#x}:{:#x}:{:#x}:{}",
            first.pool,
            WETH,
            token,
            Self::protocol_label(first.protocol),
            second.pool,
            token,
            WETH,
            Self::protocol_label(second.protocol)
        )
    }

    /// Discovers every valid post-K16 candidate and sorts exact d44 keys.
    pub fn discover(
        fixture_id: &str,
        pools: &[PreparedPoolState],
        dirty_pools: &[Address],
        cancellation: &CancellationProbe,
    ) -> Result<Vec<PairwiseCandidate>, PairwiseError> {
        if fixture_id.is_empty()
            || !fixture_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(PairwiseError::Invalid("fixture id"));
        }
        let markets = Self::rank_k16(pools, dirty_pools, cancellation)?;
        let pair_count = Self::pair_count(&markets)?;
        let mut candidates = Vec::with_capacity(pair_count);
        for market in &markets {
            for first in &market.pools {
                for second in &market.pools {
                    if first.pool == second.pool {
                        continue;
                    }
                    if !cancellation.checkpoint(Instant::now(), true) {
                        cancellation.acknowledge_drop();
                        return Err(PairwiseError::Cancelled);
                    }
                    let quote_route = |amount: &U256| {
                        let first_out = first.quote_exact_in(WETH, *amount, cancellation)?;
                        second.quote_exact_in(market.token, first_out, cancellation)
                    };
                    let size = PairwiseOptimizer::optimize(
                        quote_route,
                        &SizeBounds::default(),
                        cancellation,
                    )?;
                    let amount_out = quote_route(&size.amount)?;
                    let route = [
                        BackrunHop {
                            pool: first.pool,
                            protocol: first.protocol,
                            token_in: WETH,
                            token_out: market.token,
                            // R8: carry the chosen pool's hash-pinned sizing fee.
                            fee_pips: first.fee_pips,
                        },
                        BackrunHop {
                            pool: second.pool,
                            protocol: second.protocol,
                            token_in: market.token,
                            token_out: WETH,
                            fee_pips: second.fee_pips,
                        },
                    ];
                    candidates.push(PairwiseCandidate {
                        fixture_id: fixture_id.to_owned(),
                        directed_key: Self::directed_key(first, second, market.token),
                        route,
                        amount_in: size.amount,
                        amount_out,
                        gross_profit: size.profit,
                    });
                    if candidates.len() > MAX_CANDIDATES {
                        return Err(PairwiseError::LimitExceeded);
                    }
                }
            }
        }
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PairwiseError::Cancelled);
        }
        candidates.sort_by(|left, right| {
            left.directed_key
                .cmp(&right.directed_key)
                .then_with(|| left.amount_in.cmp(&right.amount_in))
        });
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PairwiseError::Cancelled);
        }
        Ok(candidates)
    }

    /// Selects at most one positive-gross measurement DTO by frozen canonical rank.
    pub fn select_measurement(
        processed: &ProcessedFrame,
        candidates: &[PairwiseCandidate],
        cancellation: &CancellationProbe,
    ) -> Result<Option<BackrunPlan>, PairwiseError> {
        let context = processed.measurement_context();
        if MAX_PLANS_PER_FRAME != 1 {
            return Err(PairwiseError::LimitExceeded);
        }
        if candidates.len() > MAX_CANDIDATES {
            return Err(PairwiseError::LimitExceeded);
        }
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PairwiseError::Cancelled);
        }
        let mut ranked = candidates
            .iter()
            .filter(|candidate| candidate.gross_profit.is_positive())
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .gross_profit
                .cmp(&left.gross_profit)
                .then_with(|| left.directed_key.cmp(&right.directed_key))
        });
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PairwiseError::Cancelled);
        }
        let Some(winner) = ranked.first() else { return Ok(None) };
        let gross_wide = winner.gross_profit.into_raw();
        let gross_profit = U256::checked_from_limbs_slice(gross_wide.as_limbs())
            .ok_or(PairwiseError::Overflow("gross profit"))?;
        let mut plan = BackrunPlan {
            parent_hash: context.parent_hash,
            block_number: context.block_number,
            predecessor_index: context.predecessor_index,
            payload_id: context.payload_id,
            victim: context.victim,
            route: winner.route,
            amount_in: winner.amount_in,
            amount_out: winner.amount_out,
            gross_profit,
            digest: BackrunPlanDigest(B256::ZERO),
        };
        plan.digest = MeasurementEncoder::digest(&plan)?;
        Ok(Some(plan))
    }
}

/// Canonical d44 candidate JSON bytes for cancel-false parity.
#[derive(Debug, Default, Clone, Copy)]
pub struct D44CandidateEncoder;

impl D44CandidateEncoder {
    /// Validates, sorts, and encodes candidates in the pinned d44 field order.
    pub fn encode(candidates: &[PairwiseCandidate]) -> Result<Vec<u8>, PairwiseError> {
        if candidates.len() > MAX_CANDIDATES {
            return Err(PairwiseError::LimitExceeded);
        }
        let mut sorted = candidates.iter().collect::<Vec<_>>();
        sorted.sort_by(|left, right| {
            left.directed_key
                .cmp(&right.directed_key)
                .then_with(|| left.amount_in.cmp(&right.amount_in))
        });
        let mut bytes = Vec::new();
        bytes.push(b'[');
        for (index, candidate) in sorted.into_iter().enumerate() {
            let [first, second] = candidate.route;
            let expected_key = format!(
                "{:#x}:{:#x}:{:#x}:{}>{:#x}:{:#x}:{:#x}:{}",
                first.pool,
                first.token_in,
                first.token_out,
                PairwiseEngine::protocol_label(first.protocol),
                second.pool,
                second.token_in,
                second.token_out,
                PairwiseEngine::protocol_label(second.protocol)
            );
            let expected_profit = I512::from_raw(U512::from(candidate.amount_out))
                .checked_sub(I512::from_raw(U512::from(candidate.amount_in)))
                .ok_or(PairwiseError::Overflow("candidate gross profit"))?;
            if candidate.fixture_id.is_empty()
                || !candidate
                    .fixture_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                || candidate.directed_key != expected_key
                || first.pool == second.pool
                || first.token_in != second.token_out
                || first.token_out != second.token_in
                || first.token_in != WETH
                || candidate.amount_in.is_zero()
                || candidate.gross_profit != expected_profit
            {
                return Err(PairwiseError::Invalid("canonical candidate"));
            }
            if index > 0 {
                bytes.push(b',');
            }
            let value = format!(
                concat!(
                    "{{\"amountIn\":\"{}\",\"amountOut\":\"{}\",",
                    "\"approximation\":false,\"directedKey\":\"{}\",\"error\":null,",
                    "\"fixtureId\":\"{}\",\"grossProfit\":\"{}\",\"route\":[",
                    "{{\"pool\":\"{:#x}\",\"protocol\":\"{}\",\"tokenIn\":\"{:#x}\",\"tokenOut\":\"{:#x}\"}},",
                    "{{\"pool\":\"{:#x}\",\"protocol\":\"{}\",\"tokenIn\":\"{:#x}\",\"tokenOut\":\"{:#x}\"}}]}}"
                ),
                candidate.amount_in,
                candidate.amount_out,
                candidate.directed_key,
                candidate.fixture_id,
                candidate.gross_profit,
                first.pool,
                PairwiseEngine::protocol_label(first.protocol),
                first.token_in,
                first.token_out,
                second.pool,
                PairwiseEngine::protocol_label(second.protocol),
                second.token_in,
                second.token_out,
            );
            let new_len =
                bytes.len().checked_add(value.len()).ok_or(PairwiseError::LimitExceeded)?;
            if new_len > crate::MAX_CANONICAL_BYTES {
                return Err(PairwiseError::LimitExceeded);
            }
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(b"]\n");
        Ok(bytes)
    }
}

/// Pinned d44 fatal contract error shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct D44ContractError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Fixture identity, possibly empty for parse failures.
    pub fixture_id: String,
    /// Stable JSON-style error path.
    pub path: String,
    /// Human-readable deterministic detail.
    pub detail: String,
}

/// Canonical d44 error JSON bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct D44ErrorEncoder;

impl D44ErrorEncoder {
    /// Appends one JSON string using serde-compatible compact escapes.
    pub fn push_json_string(output: &mut String, value: &str) {
        output.push('"');
        for character in value.chars() {
            match character {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\u{08}' => output.push_str("\\b"),
                '\u{0c}' => output.push_str("\\f"),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                character if character <= '\u{1f}' => {
                    write!(output, "\\u{:04x}", u32::from(character))
                        .expect("String writes are infallible");
                }
                character => output.push(character),
            }
        }
        output.push('"');
    }

    /// Encodes fields in canonical alphabetical key order with one trailing newline.
    pub fn encode(error: &D44ContractError) -> Result<Vec<u8>, PairwiseError> {
        if error.code.is_empty()
            || !error
                .code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || !error
                .fixture_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(PairwiseError::Invalid("canonical error"));
        }
        let mut output = String::from("{\"code\":");
        Self::push_json_string(&mut output, &error.code);
        output.push_str(",\"detail\":");
        Self::push_json_string(&mut output, &error.detail);
        output.push_str(",\"fixtureId\":");
        Self::push_json_string(&mut output, &error.fixture_id);
        output.push_str(",\"path\":");
        Self::push_json_string(&mut output, &error.path);
        output.push_str("}\n");
        if output.len() > crate::MAX_CANONICAL_BYTES {
            return Err(PairwiseError::LimitExceeded);
        }
        Ok(output.into_bytes())
    }
}

/// Canonical self-excluding binary measurement encoding.
#[derive(Debug, Default, Clone, Copy)]
pub struct MeasurementEncoder;

impl MeasurementEncoder {
    /// Encodes fixed-width measurement fields while excluding the digest field.
    pub fn encode(plan: &BackrunPlan) -> Result<Vec<u8>, PairwiseError> {
        if plan.route[0].token_in != plan.route[1].token_out
            || plan.route[0].token_out != plan.route[1].token_in
            || plan.route[0].pool == plan.route[1].pool
            || plan.amount_out <= plan.amount_in
            || plan.gross_profit != plan.amount_out - plan.amount_in
        {
            return Err(PairwiseError::Invalid("measurement plan"));
        }
        // Domain bumped v1 -> v2 when `fee_pips` entered the digest preimage (R8):
        // a v1 consumer must never accept a v2 digest and vice versa.
        let mut encoder = CanonicalEncoder::with_domain(b"mev-trader-backrun-plan-v2")
            .map_err(|_| PairwiseError::LimitExceeded)?;
        encoder.push_b256(plan.parent_hash).map_err(|_| PairwiseError::LimitExceeded)?;
        encoder
            .push_bytes(&plan.block_number.to_be_bytes())
            .map_err(|_| PairwiseError::LimitExceeded)?;
        encoder
            .push_bytes(&plan.predecessor_index.to_be_bytes())
            .map_err(|_| PairwiseError::LimitExceeded)?;
        encoder
            .push_bytes(plan.payload_id.0.as_slice())
            .map_err(|_| PairwiseError::LimitExceeded)?;
        encoder.push_b256(plan.victim).map_err(|_| PairwiseError::LimitExceeded)?;
        for hop in plan.route {
            encoder.push_address(hop.pool).map_err(|_| PairwiseError::LimitExceeded)?;
            encoder.push_u8(hop.protocol as u8).map_err(|_| PairwiseError::LimitExceeded)?;
            encoder.push_address(hop.token_in).map_err(|_| PairwiseError::LimitExceeded)?;
            encoder.push_address(hop.token_out).map_err(|_| PairwiseError::LimitExceeded)?;
            // R8: fee_pips is part of the digest preimage so a tampered fee (which
            // would change the executor's derived feeBps) fails self-validation.
            encoder.push_u32(hop.fee_pips).map_err(|_| PairwiseError::LimitExceeded)?;
        }
        encoder.push_u256(plan.amount_in).map_err(|_| PairwiseError::LimitExceeded)?;
        encoder.push_u256(plan.amount_out).map_err(|_| PairwiseError::LimitExceeded)?;
        encoder.push_u256(plan.gross_profit).map_err(|_| PairwiseError::LimitExceeded)?;
        Ok(encoder.finish())
    }

    /// Computes the canonical measurement digest while excluding its self-field.
    pub fn digest(plan: &BackrunPlan) -> Result<BackrunPlanDigest, PairwiseError> {
        Ok(BackrunPlanDigest(CanonicalDigest::sha256(&Self::encode(plan)?)))
    }

    /// Validates the stored self-excluding digest.
    pub fn validate(plan: &BackrunPlan) -> Result<(), PairwiseError> {
        if Self::digest(plan)? == plan.digest {
            Ok(())
        } else {
            Err(PairwiseError::Invalid("measurement digest"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{CancellationToken, GlobalLifecycle, TaskState, frame};

    fn probe() -> CancellationProbe {
        CancellationProbe::new(
            Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
            Arc::new(GlobalLifecycle::default()),
        )
    }

    fn candidate(directed_key: &str, first_pool: u8, gross_profit: i64) -> PairwiseCandidate {
        let token = Address::with_last_byte(0xaa);
        PairwiseCandidate {
            fixture_id: directed_key.to_owned(),
            directed_key: directed_key.to_owned(),
            route: [
                BackrunHop {
                    pool: Address::with_last_byte(first_pool),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: 3_000,
                },
                BackrunHop {
                    pool: Address::with_last_byte(first_pool + 1),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: 3_000,
                },
            ],
            amount_in: U256::from(10),
            amount_out: U256::from(
                u64::try_from(10 + gross_profit).expect("nonnegative fixture output"),
            ),
            gross_profit: I512::try_from(gross_profit).expect("fixture gross"),
        }
    }

    fn v2(pool: u8, token: Address, weth_reserve: u64) -> PreparedPoolState {
        PreparedPoolState {
            pool: Address::with_last_byte(pool),
            protocol: ExactProtocol::UniswapV2,
            token0: WETH,
            token1: token,
            decimals0: 18,
            decimals1: 18,
            fee_pips: 3_000,
            quote: PreparedPoolQuote::ConstantProduct {
                reserve0: U256::from(weth_reserve),
                reserve1: U256::from(1_000_000_000_000_000_000u64),
            },
        }
    }

    #[test]
    fn k16_ranks_weight_descending_then_address_and_caps_pair_count() {
        let token = Address::with_last_byte(0xaa);
        let pools = (1..=17)
            .map(|pool| v2(pool, token, if pool <= 2 { 99 } else { u64::from(pool) }))
            .collect::<Vec<_>>();
        let markets = PairwiseEngine::rank_k16(&pools, &[pools[16].pool], &probe()).expect("K16");
        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].pools.len(), K16);
        assert_eq!(markets[0].pools[0].pool, Address::with_last_byte(1));
        assert_eq!(markets[0].pools[1].pool, Address::with_last_byte(2));
        assert_eq!(PairwiseEngine::pair_count(&markets), Ok(K16 * (K16 - 1)));

        let sparse = (0..=MAX_ACTIVATED_TOKENS)
            .map(|index| {
                let mut token_bytes = [0u8; 20];
                token_bytes[0] = 1;
                token_bytes[19] = u8::try_from(index + 1).expect("token identity");
                v2(u8::try_from(index + 100).expect("pool identity"), Address::from(token_bytes), 1)
            })
            .collect::<Vec<_>>();
        let sparse_dirty = sparse.iter().map(|pool| pool.pool).collect::<Vec<_>>();
        let sparse_markets =
            PairwiseEngine::rank_k16(&sparse, &sparse_dirty, &probe()).expect("sparse markets");
        assert_eq!(sparse_markets.len(), MAX_ACTIVATED_TOKENS + 1);
        assert_eq!(PairwiseEngine::pair_count(&sparse_markets), Ok(0));
    }

    #[test]
    fn maximum_k16_shape_is_exactly_7680_and_within_8192() {
        let mut pools = Vec::with_capacity(MAX_POOLS);
        let mut dirty = Vec::with_capacity(MAX_ACTIVATED_TOKENS);
        for token_index in 0..MAX_ACTIVATED_TOKENS {
            let mut token_bytes = [0u8; 20];
            token_bytes[0] = 1;
            token_bytes[19] = u8::try_from(token_index + 1).expect("token byte");
            let token = Address::from(token_bytes);
            for rank in 0..K16 {
                let identity = u16::try_from(token_index * K16 + rank + 1).expect("pool identity");
                let mut pool_bytes = [0u8; 20];
                pool_bytes[18..].copy_from_slice(&identity.to_be_bytes());
                let state = PreparedPoolState {
                    pool: Address::from(pool_bytes),
                    protocol: ExactProtocol::UniswapV2,
                    token0: WETH,
                    token1: token,
                    decimals0: 18,
                    decimals1: 18,
                    fee_pips: 3_000,
                    quote: PreparedPoolQuote::ConstantProduct {
                        reserve0: U256::from(K16 - rank),
                        reserve1: U256::from(1_000_000u64),
                    },
                };
                if rank == 0 {
                    dirty.push(state.pool);
                }
                pools.push(state);
            }
        }
        let markets = PairwiseEngine::rank_k16(&pools, &dirty, &probe()).expect("maximum K16");
        assert_eq!(markets.len(), MAX_ACTIVATED_TOKENS);
        assert_eq!(PairwiseEngine::pair_count(&markets), Ok(32 * 16 * 15));
        const { assert!(32 * 16 * 15 <= MAX_PAIRS) };
    }

    #[test]
    fn d44_frozen_v2_candidate_bytes_match_exactly() {
        let token = Address::with_last_byte(0xaa);
        let first = v2(1, token, 1_000_000_000_000_000_000);
        let second = v2(2, token, 1_000_000_000_000_000_000);
        let route = [
            BackrunHop {
                pool: first.pool,
                protocol: first.protocol,
                token_in: WETH,
                token_out: token,
                fee_pips: first.fee_pips,
            },
            BackrunHop {
                pool: second.pool,
                protocol: second.protocol,
                token_in: token,
                token_out: WETH,
                fee_pips: second.fee_pips,
            },
        ];
        let candidate = PairwiseCandidate {
            fixture_id: "ties".to_owned(),
            directed_key: PairwiseEngine::directed_key(&first, &second, token),
            route,
            amount_in: U256::from(1_000_000_000_000u64),
            amount_out: U256::from(994_008_999_998u64),
            gross_profit: I512::try_from(-5_991_000_002i64).expect("signed fixture"),
        };
        let expected = format!(
            concat!(
                "[{{\"amountIn\":\"1000000000000\",\"amountOut\":\"994008999998\",",
                "\"approximation\":false,\"directedKey\":\"{}\",\"error\":null,",
                "\"fixtureId\":\"ties\",\"grossProfit\":\"-5991000002\",\"route\":[",
                "{{\"pool\":\"{:#x}\",\"protocol\":\"uniswap_v2\",\"tokenIn\":\"{:#x}\",\"tokenOut\":\"{:#x}\"}},",
                "{{\"pool\":\"{:#x}\",\"protocol\":\"uniswap_v2\",\"tokenIn\":\"{:#x}\",\"tokenOut\":\"{:#x}\"}}]}}]\n"
            ),
            candidate.directed_key, first.pool, WETH, token, second.pool, token, WETH,
        );
        assert_eq!(D44CandidateEncoder::encode(&[candidate]), Ok(expected.into_bytes()));
    }

    /// Builds a valid two-hop measurement plan whose hops carry `fee0`/`fee1`.
    fn measurement_plan(token: Address, fee0: u32, fee1: u32) -> BackrunPlan {
        BackrunPlan {
            parent_hash: B256::repeat_byte(0x11),
            block_number: 100,
            predecessor_index: 2,
            payload_id: PayloadId::new([7u8; 8]),
            victim: B256::repeat_byte(0x22),
            route: [
                BackrunHop {
                    pool: Address::with_last_byte(1),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: fee0,
                },
                BackrunHop {
                    pool: Address::with_last_byte(2),
                    protocol: ExactProtocol::UniswapV2,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: fee1,
                },
            ],
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_100u64),
            gross_profit: U256::from(100u64),
            digest: BackrunPlanDigest(B256::ZERO),
        }
    }

    #[test]
    fn discover_carries_each_prepared_pool_fee_pips_into_the_route() {
        // Two activated pools for one token with DISTINCT sizing fees; discover must
        // inject each chosen pool's own fee into its route hop (R8 fee-SOURCE carry).
        let token = Address::with_last_byte(0xbb);
        let pool_a_addr = Address::with_last_byte(1);
        let mut a = v2(1, token, 1_000_000_000_000_000_000);
        a.fee_pips = 3_000; // 0.30%
        let mut b = v2(2, token, 1_000_000_000_000_000_000);
        b.fee_pips = 500; // 0.05%
        let pools = vec![a, b];
        let candidates = PairwiseEngine::discover("fee-carry", &pools, &[pool_a_addr], &probe())
            .expect("discover");
        assert!(!candidates.is_empty(), "expected at least one directed pair");
        let fee_of = |addr: Address| if addr == pool_a_addr { 3_000 } else { 500 };
        for candidate in &candidates {
            let [first, second] = candidate.route;
            assert_eq!(first.fee_pips, fee_of(first.pool), "first hop fee not carried");
            assert_eq!(second.fee_pips, fee_of(second.pool), "second hop fee not carried");
        }

        let failing_token = Address::with_last_byte(0xcc);
        let failing_first = v2(3, failing_token, 1);
        let failing_dirty = failing_first.pool;
        let failing_second = v2(4, failing_token, 1);
        let mut mixed = pools;
        mixed.extend([failing_first, failing_second]);
        assert!(matches!(
            PairwiseEngine::discover(
                "fatal-pair-is-frame-fatal",
                &mixed,
                &[pool_a_addr, failing_dirty],
                &probe(),
            ),
            Err(PairwiseError::Invalid(_) | PairwiseError::Exhausted(_))
        ));
    }

    #[test]
    fn measurement_digest_binds_fee_pips_under_domain_v2() {
        let token = Address::with_last_byte(0xcc);
        let base = measurement_plan(token, 3_000, 3_000);

        // The canonical preimage is domain-tagged v2 (bumped when fee entered it).
        let preimage = MeasurementEncoder::encode(&base).expect("encode");
        let domain = b"mev-trader-backrun-plan-v2";
        assert_eq!(&preimage[4..4 + domain.len()], domain, "digest domain must be v2");

        // A self-consistent plan validates.
        let mut bound = base.clone();
        bound.digest = MeasurementEncoder::digest(&base).expect("digest");
        assert!(MeasurementEncoder::validate(&bound).is_ok());

        // Changing ONLY a hop fee changes the digest — fee_pips is bound.
        let other_fee = measurement_plan(token, 3_000, 500);
        assert_ne!(
            MeasurementEncoder::digest(&base).expect("digest"),
            MeasurementEncoder::digest(&other_fee).expect("digest"),
            "fee_pips is not bound into the digest",
        );

        // Post-hoc fee tamper (keeping the old digest) fails self-validation.
        let mut tampered = bound.clone();
        tampered.route[0].fee_pips = 500;
        assert_eq!(
            MeasurementEncoder::validate(&tampered),
            Err(PairwiseError::Invalid("measurement digest")),
        );
    }

    #[test]
    fn d44_candidate_wire_is_byte_invariant_under_fee_pips() {
        // The d44 candidate wire feeds graph-arb parity and MUST be byte-identical
        // regardless of fee_pips (fee lives only in the plan digest, never here).
        let token = Address::with_last_byte(0xaa);
        let first = v2(1, token, 1_000_000_000_000_000_000);
        let second = v2(2, token, 1_000_000_000_000_000_000);
        let make = |fee0: u32, fee1: u32| PairwiseCandidate {
            fixture_id: "fee-invariant".to_owned(),
            directed_key: PairwiseEngine::directed_key(&first, &second, token),
            route: [
                BackrunHop {
                    pool: first.pool,
                    protocol: first.protocol,
                    token_in: WETH,
                    token_out: token,
                    fee_pips: fee0,
                },
                BackrunHop {
                    pool: second.pool,
                    protocol: second.protocol,
                    token_in: token,
                    token_out: WETH,
                    fee_pips: fee1,
                },
            ],
            amount_in: U256::from(1_000_000_000_000u64),
            amount_out: U256::from(994_008_999_998u64),
            gross_profit: I512::try_from(-5_991_000_002i64).expect("signed fixture"),
        };
        let low = D44CandidateEncoder::encode(&[make(3_000, 3_000)]).expect("low fee wire");
        let high = D44CandidateEncoder::encode(&[make(10_000, 1)]).expect("high fee wire");
        assert_eq!(low, high, "fee_pips must not change the d44 candidate wire");
    }

    #[test]
    fn d44_error_bytes_sort_fields_and_escape_strings_exactly() {
        let error = D44ContractError {
            code: "schema_invalid".to_owned(),
            fixture_id: "case-1".to_owned(),
            path: "$.pool".to_owned(),
            detail: "bad \"value\"\n".to_owned(),
        };
        assert_eq!(
            D44ErrorEncoder::encode(&error),
            Ok(b"{\"code\":\"schema_invalid\",\"detail\":\"bad \\\"value\\\"\\n\",\"fixtureId\":\"case-1\",\"path\":\"$.pool\"}\n".to_vec())
        );
    }

    #[test]
    fn issue_76_quote_literals_and_corrected_source_oid_are_locked() {
        let bitcoin =
            "0x2a06a17cbc6d0032cac2c6696da90f29d39a1a29".parse::<Address>().expect("BITCOIN");
        let usdc = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".parse::<Address>().expect("USDC");
        let pool = PreparedPoolState {
            pool: "0xe966fbc4694f91d7798139775e319c0592169466".parse::<Address>().expect("pool"),
            protocol: ExactProtocol::UniswapV3,
            token0: bitcoin,
            token1: usdc,
            decimals0: 8,
            decimals1: 6,
            fee_pips: 10_000,
            quote: PreparedPoolQuote::V3 {
                sqrt_price_x96: U256::from_str_radix("943813745190153014431461982", 10)
                    .expect("sqrt price"),
                liquidity: U256::from(2_175_831_642_771u64),
                tick: -88_608,
                tick_spacing: 200,
                ticks: [
                    (-93_200, 4_153_717_093i64),
                    (-90_800, 2_175_831_642_771i64),
                    (-89_400, -2_278_700_913i64),
                    (-89_200, -1_875_016_180i64),
                    (-77_200, -2_175_831_642_771i64),
                    (-74_800, 7_364_645_012i64),
                    (-72_600, -7_364_645_012i64),
                ]
                .map(|(tick, liquidity_net)| PairwiseV3Tick {
                    tick,
                    liquidity_net: I256::try_from(liquidity_net).expect("liquidity net"),
                })
                .to_vec(),
            },
        };
        let engine = pool
            .quote_exact_in(bitcoin, U256::from(8_753_544_975u64), &probe())
            .expect("issue 76 quote");
        let observed = U256::from(1_216_314u64);
        assert_eq!(engine, U256::from(1_229_736u64));
        assert_eq!(engine - observed, U256::from(13_422u64));
        assert_eq!("d55983dbc8d075c6ba8012d5e0b40501122147ee".len(), 40);
    }

    #[test]
    fn measurement_selection_rejects_empty_and_nonpositive_candidates() {
        let processed = frame::test_utils::processed_frame();
        assert_eq!(PairwiseEngine::select_measurement(&processed, &[], &probe()), Ok(None));
        assert_eq!(
            PairwiseEngine::select_measurement(
                &processed,
                &[candidate("zero", 1, 0), candidate("negative", 3, -1)],
                &probe(),
            ),
            Ok(None)
        );
    }

    #[test]
    fn measurement_selection_is_proof_bound_deterministic_and_max_one() {
        let processed = frame::test_utils::processed_frame();
        let context = *processed.measurement_context();
        let later = candidate("z-route", 3, 2);
        let earlier = candidate("a-route", 1, 2);

        let plan = PairwiseEngine::select_measurement(
            &processed,
            &[later.clone(), earlier.clone()],
            &probe(),
        )
        .expect("selection")
        .expect("one plan");
        let reversed =
            PairwiseEngine::select_measurement(&processed, &[earlier.clone(), later], &probe())
                .expect("selection")
                .expect("one plan");

        assert_eq!(plan.digest, reversed.digest);
        assert_eq!(plan.parent_hash, context.parent_hash);
        assert_eq!(plan.block_number, context.block_number);
        assert_eq!(plan.predecessor_index, context.predecessor_index);
        assert_eq!(plan.payload_id, context.payload_id);
        assert_eq!(plan.victim, context.victim);
        assert_eq!(plan.route, earlier.route);
        assert_eq!(plan.amount_in, earlier.amount_in);
        assert_eq!(plan.amount_out, earlier.amount_out);
        assert_eq!(plan.gross_profit, U256::from(2));

        let lower_profit = candidate("a-lower-profit", 1, 1);
        let higher_profit = candidate("z-higher-profit", 3, 3);
        let profit_ranked = PairwiseEngine::select_measurement(
            &processed,
            &[lower_profit, higher_profit.clone()],
            &probe(),
        )
        .expect("profit-ranked selection")
        .expect("one higher-profit plan");
        assert_eq!(profit_ranked.route, higher_profit.route);
        assert_eq!(profit_ranked.gross_profit, U256::from(3));
    }

    #[test]
    fn measurement_digest_validates_and_excludes_only_its_self_field() {
        let processed = frame::test_utils::processed_frame();
        let plan =
            PairwiseEngine::select_measurement(&processed, &[candidate("winner", 1, 2)], &probe())
                .expect("selection")
                .expect("plan");

        MeasurementEncoder::validate(&plan).expect("digest");
        let encoded = MeasurementEncoder::encode(&plan).expect("canonical bytes");
        let expected = plan.digest;
        let mut self_mutated = plan.clone();
        self_mutated.digest = BackrunPlanDigest(B256::with_last_byte(99));
        assert_eq!(MeasurementEncoder::encode(&self_mutated), Ok(encoded));
        assert_eq!(MeasurementEncoder::digest(&self_mutated), Ok(expected));

        let mut bound_field_mutated = plan;
        bound_field_mutated.victim = B256::with_last_byte(99);
        assert_ne!(MeasurementEncoder::digest(&bound_field_mutated), Ok(expected));
        assert!(MeasurementEncoder::validate(&bound_field_mutated).is_err());
    }

    #[test]
    fn measurement_selection_drops_cancellation_after_proof() {
        let processed = frame::test_utils::processed_frame();
        let token = Arc::new(CancellationToken::with_approved_deadline(Instant::now()));
        let cancellation =
            CancellationProbe::new(Arc::clone(&token), Arc::new(GlobalLifecycle::default()));
        assert!(token.request_cancel());

        assert_eq!(
            PairwiseEngine::select_measurement(
                &processed,
                &[candidate("winner", 1, 2)],
                &cancellation,
            ),
            Err(PairwiseError::Cancelled)
        );
        assert_eq!(token.state(), TaskState::DroppedAcked);
    }
}
