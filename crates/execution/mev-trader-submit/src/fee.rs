//! Fee-parity conversion at the execution ABI boundary.
//!
//! Implements `.omc/specs/deep-dive-fee-parity-contract-2026-07-19.md` §3: the
//! canonical sizing unit is `feePips` (denominator `1e6`); `feeBps` (`1e4`) is a
//! derived representation that appears ONLY when encoding executor calldata.
//!
//! * Constant-product hops (`UniswapV2`, `AerodromeVolatile`) apply `feeBps`
//!   on-chain, so the conversion must be lossless: `fee_pips % 100 == 0` is
//!   required (fail-closed) and `feeBps = fee_pips / 100` (§3.4-1).
//! * Self-applying hops (`UniswapV3` incl. Slipstream CL, `AerodromeStable`)
//!   ignore the calldata `feeBps` — the pool applies its own fee — so a single
//!   canonical `feeBps = 0` is emitted (§3.4-2).
//! * The [`ExactProtocol`] enum is closed (4 variants), so the match is total by
//!   construction — no `unknown`/5th-protocol silent pass is possible (§3.4-4).

use base_mev_trader::{ExactProtocol, FEE_DENOMINATOR};

/// A fee-parity conversion or validation failure. The encoder is fail-closed: any
/// error aborts calldata assembly rather than emitting a mispriced hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeParityError {
    /// `fee_pips` exceeded the canonical denominator (`1e6`).
    OutOfRange {
        /// The offending sizing fee in pips.
        fee_pips: u32,
    },
    /// A constant-product hop carried a fractional-bps fee (`fee_pips % 100 != 0`)
    /// that a `uint24 feeBps` cannot represent without loss.
    FractionalBps {
        /// The offending sizing fee in pips.
        fee_pips: u32,
    },
}

impl core::fmt::Display for FeeParityError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfRange { fee_pips } => {
                write!(formatter, "fee_pips {fee_pips} exceeds denominator {FEE_DENOMINATOR}")
            }
            Self::FractionalBps { fee_pips } => write!(
                formatter,
                "constant-product hop requires fee_pips ({fee_pips}) divisible by 100"
            ),
        }
    }
}

impl core::error::Error for FeeParityError {}

/// Convert a canonical sizing fee (`fee_pips`, denominator `1e6`) to the executor
/// ABI `feeBps` (denominator `1e4`) for a given hop protocol, applying the
/// fee-parity contract §3.4 guards. The returned value always fits `uint24`.
///
/// This is the SINGLE conversion point: callers must never populate a hop's
/// `feeBps` from any other source.
pub const fn fee_bps_for_executor(
    protocol: ExactProtocol,
    fee_pips: u32,
) -> Result<u32, FeeParityError> {
    // §3.4-3 common range guard (mirrors Rust sizing `pairwise.rs` and TS
    // `assertFeeBps`). `fee_pips` is unsigned, so only the upper bound matters.
    if fee_pips > FEE_DENOMINATOR {
        return Err(FeeParityError::OutOfRange { fee_pips });
    }
    match protocol {
        // §3.4-1: constant-product pools consume `feeBps` on-chain — lossless only.
        ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile => {
            if !fee_pips.is_multiple_of(100) {
                return Err(FeeParityError::FractionalBps { fee_pips });
            }
            Ok(fee_pips / 100)
        }
        // §3.4-2: pool self-applies its fee; the calldata value is non-load-bearing
        // and canonicalized to 0 (never a truncated/misleading derived value).
        ExactProtocol::UniswapV3 | ExactProtocol::AerodromeStable => Ok(0),
    }
}
