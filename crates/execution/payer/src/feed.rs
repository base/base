//! Pluggable external price-feed decoding for ERC-8168 payer pricing.
//!
//! A [`FeedConfig`] names the oracle contract, the method [`selector`] that
//! supplies the price, and the [`AnswerShape`] / [`FeedDirection`] / decimals
//! that describe how to interpret its return. The reader layer performs the
//! actual `STATICCALL` and hands the raw return bytes to
//! [`AnswerShape::decode`], producing a [`FeedReading`]; [`FeedConfig::rate`]
//! then turns that reading into an exact [`Rate`] after a staleness check.
//!
//! [`selector`]: FeedConfig::selector

use alloy_primitives::{Address, U256};

use crate::{error::PricingError, rate::Rate};

/// Direction of an external price feed's answer relative to the payment token
/// and the native asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedDirection {
    /// The answer is native value (`10^answer_decimals`-scaled wei-equivalent)
    /// per **one whole token** — e.g. a `TOKEN / ETH` feed.
    NativePerToken,
    /// The answer is whole tokens (`10^answer_decimals`-scaled) per **one whole
    /// native unit** — e.g. an `ETH / TOKEN` feed.
    TokenPerNative,
}

/// Layout of an external price feed's ABI-encoded return, so the node knows
/// which 32-byte word carries the answer and (when present) the update
/// timestamp used for staleness enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerShape {
    /// A single 32-byte word carrying the answer (e.g. a bare `latestAnswer()`
    /// returning `int256`). No embedded timestamp, so staleness cannot be
    /// enforced from the return and [`FeedConfig::staleness_bound`] MUST be 0.
    SingleWord,
    /// Chainlink `AggregatorV3Interface.latestRoundData()` returning
    /// `(uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt,
    /// uint80 answeredInRound)`: the answer is word index 1 and `updatedAt` is
    /// word index 3.
    ChainlinkRoundData,
}

impl AnswerShape {
    /// Minimum number of return-data bytes this shape reads.
    pub const fn min_len(&self) -> usize {
        match self {
            Self::SingleWord => 32,
            Self::ChainlinkRoundData => 32 * 5,
        }
    }

    /// Decodes raw oracle return bytes into a [`FeedReading`] per this shape.
    pub fn decode(&self, data: &[u8]) -> Result<FeedReading, PricingError> {
        let min_len = self.min_len();
        if data.len() < min_len {
            return Err(PricingError::ShortReturnData { expected: min_len, got: data.len() });
        }
        match self {
            Self::SingleWord => Ok(FeedReading { answer: Self::word(data, 0)?, updated_at: None }),
            Self::ChainlinkRoundData => {
                let answer = Self::word(data, 1)?;
                let updated_at_word = Self::word(data, 3)?;
                if updated_at_word > U256::from(u64::MAX) {
                    return Err(PricingError::Overflow);
                }
                Ok(FeedReading { answer, updated_at: Some(updated_at_word.to::<u64>()) })
            }
        }
    }

    /// Reads the 32-byte word at `index` as a big-endian `U256`.
    fn word(data: &[u8], index: usize) -> Result<U256, PricingError> {
        let end = (index + 1) * 32;
        let slice = data
            .get(index * 32..end)
            .ok_or(PricingError::ShortReturnData { expected: end, got: data.len() })?;
        Ok(U256::from_be_slice(slice))
    }
}

/// A decoded price-feed reading: the raw `answer` word and, when the shape
/// carries it, the `updatedAt` timestamp used for staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedReading {
    /// Raw answer word (interpreted as a non-negative `int256`).
    pub answer: U256,
    /// `updatedAt` timestamp in unix seconds, when the shape provides one.
    pub updated_at: Option<u64>,
}

/// On-chain configuration for a feed-backed token price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedConfig {
    /// Oracle contract queried for the price.
    pub oracle: Address,
    /// 4-byte method selector that supplies the price (e.g. `latestRoundData`).
    pub selector: [u8; 4],
    /// Layout of the oracle's return, describing how to decode it.
    pub answer_shape: AnswerShape,
    /// Direction of the answer relative to the token and native asset.
    pub direction: FeedDirection,
    /// Decimal scaling of the feed answer (`answer = price × 10^answer_decimals`).
    pub answer_decimals: u8,
    /// ERC-20 decimals of the payment token.
    pub token_decimals: u8,
    /// Maximum permitted answer age in seconds; `0` disables the check (only
    /// valid for shapes carrying no timestamp — see [`AnswerShape::SingleWord`]).
    pub staleness_bound: u64,
}

impl FeedConfig {
    /// Resolves a [`FeedReading`] to an exact [`Rate`] (token atomic units per
    /// native wei), enforcing positivity and the configured staleness bound.
    pub fn rate(&self, reading: FeedReading, now: u64) -> Result<Rate, PricingError> {
        // Positivity: reject zero and negative int256 (top bit set).
        if reading.answer.is_zero() || reading.answer.bit(255) {
            return Err(PricingError::NonPositiveAnswer);
        }

        if self.staleness_bound > 0 {
            match reading.updated_at {
                None => return Err(PricingError::StalenessUnsupported),
                Some(updated_at) => {
                    let age = now.saturating_sub(updated_at);
                    if age > self.staleness_bound {
                        return Err(PricingError::StaleAnswer { age, bound: self.staleness_bound });
                    }
                }
            }
        }

        let p = reading.answer;
        let ten_dt = Self::pow10(self.token_decimals)?;
        let ten_da = Self::pow10(self.answer_decimals)?;
        let ten_18 = Self::pow10(18)?;

        // Derivations (native has 18 decimals):
        //   NativePerToken: 1 token = P/10^da native → atomic-per-wei =
        //     10^(dt+da) / (P × 10^18).
        //   TokenPerNative: 1 native = Q/10^da tokens → atomic-per-wei =
        //     (Q × 10^dt) / 10^(da+18).
        let rate = match self.direction {
            FeedDirection::NativePerToken => Rate::new(
                ten_dt.checked_mul(ten_da).ok_or(PricingError::Overflow)?,
                p.checked_mul(ten_18).ok_or(PricingError::Overflow)?,
            ),
            FeedDirection::TokenPerNative => Rate::new(
                p.checked_mul(ten_dt).ok_or(PricingError::Overflow)?,
                ten_da.checked_mul(ten_18).ok_or(PricingError::Overflow)?,
            ),
        };
        Ok(rate)
    }

    /// `10^exp` as a `U256`, or [`PricingError::DecimalsTooLarge`] on overflow.
    fn pow10(exp: u8) -> Result<U256, PricingError> {
        U256::from(10u8)
            .checked_pow(U256::from(exp))
            .ok_or(PricingError::DecimalsTooLarge(exp))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    /// Builds ABI return data from a list of `U256` words.
    fn words(words: &[U256]) -> Vec<u8> {
        let mut out = Vec::with_capacity(words.len() * 32);
        for w in words {
            out.extend_from_slice(&w.to_be_bytes::<32>());
        }
        out
    }

    fn native_per_token(staleness_bound: u64) -> FeedConfig {
        FeedConfig {
            oracle: address!("0x0000000000000000000000000000000000000abc"),
            selector: [0x00, 0x00, 0x00, 0x00],
            answer_shape: AnswerShape::ChainlinkRoundData,
            direction: FeedDirection::NativePerToken,
            answer_decimals: 18,
            token_decimals: 6,
            staleness_bound,
        }
    }

    #[test]
    fn single_word_decode() {
        let data = words(&[U256::from(1234u64)]);
        let reading = AnswerShape::SingleWord.decode(&data).unwrap();
        assert_eq!(reading, FeedReading { answer: U256::from(1234u64), updated_at: None });
    }

    #[test]
    fn chainlink_round_data_decode() {
        // roundId, answer, startedAt, updatedAt, answeredInRound
        let data = words(&[
            U256::from(42u64),
            U256::from(400_000_000_000_000u64), // 4e14
            U256::from(1000u64),
            U256::from(1234u64),
            U256::from(42u64),
        ]);
        let reading = AnswerShape::ChainlinkRoundData.decode(&data).unwrap();
        assert_eq!(
            reading,
            FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: Some(1234) }
        );
    }

    #[test]
    fn short_return_data_is_rejected() {
        let data = words(&[U256::from(1u64)]); // 32 bytes, need 160
        let err = AnswerShape::ChainlinkRoundData.decode(&data).unwrap_err();
        assert_eq!(err, PricingError::ShortReturnData { expected: 160, got: 32 });
    }

    #[test]
    fn native_per_token_rate_matches_hand_computation() {
        // 1 token = 4e-4 native (ETH ≈ $2500, token ≈ $1), answer_decimals 18.
        let cfg = native_per_token(0);
        let reading = FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: None };
        let rate = cfg.rate(reading, 0).unwrap();
        // atomic-per-wei = 10^(6+18) / (4e14 × 10^18) = 1e24 / 4e32.
        // 1 ETH (1e18 wei) → 2.5e9 atomic = 2500 USDC.
        let amount = rate.payment_amount(1_000_000_000, 1_000_000_000, 0).unwrap();
        assert_eq!(amount, U256::from(2_500_000_000u64));
    }

    #[test]
    fn token_per_native_rate_matches_native_per_token() {
        // Same economics, expressed the other way: 1 ETH = 2500 tokens.
        let cfg = FeedConfig {
            direction: FeedDirection::TokenPerNative,
            ..native_per_token(0)
        };
        // Q = 2500 × 10^18.
        let q = U256::from(2500u64) * U256::from(10u64).pow(U256::from(18u8));
        let rate = cfg.rate(FeedReading { answer: q, updated_at: None }, 0).unwrap();
        let amount = rate.payment_amount(1_000_000_000, 1_000_000_000, 0).unwrap();
        assert_eq!(amount, U256::from(2_500_000_000u64));
    }

    #[test]
    fn zero_answer_is_rejected() {
        let cfg = native_per_token(0);
        let err = cfg.rate(FeedReading { answer: U256::ZERO, updated_at: None }, 0).unwrap_err();
        assert_eq!(err, PricingError::NonPositiveAnswer);
    }

    #[test]
    fn negative_answer_is_rejected() {
        let cfg = native_per_token(0);
        // Top bit set → negative int256.
        let neg = U256::from(1u8) << 255;
        let err = cfg.rate(FeedReading { answer: neg, updated_at: None }, 0).unwrap_err();
        assert_eq!(err, PricingError::NonPositiveAnswer);
    }

    #[test]
    fn fresh_answer_within_bound_is_accepted() {
        let cfg = native_per_token(60);
        let reading = FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: Some(100) };
        assert!(cfg.rate(reading, 140).is_ok());
    }

    #[test]
    fn stale_answer_is_rejected() {
        let cfg = native_per_token(60);
        let reading = FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: Some(100) };
        let err = cfg.rate(reading, 200).unwrap_err();
        assert_eq!(err, PricingError::StaleAnswer { age: 100, bound: 60 });
    }

    #[test]
    fn staleness_bound_without_timestamp_is_rejected() {
        let cfg = FeedConfig { answer_shape: AnswerShape::SingleWord, ..native_per_token(60) };
        let reading = FeedReading { answer: U256::from(400_000_000_000_000u64), updated_at: None };
        assert_eq!(cfg.rate(reading, 0).unwrap_err(), PricingError::StalenessUnsupported);
    }
}
