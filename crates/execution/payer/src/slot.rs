//! Slot-based (`SLOAD`) price reads for ERC-8168 payer pricing.
//!
//! The builder-operated payer prices tokens deterministically against the
//! pending build state: instead of executing an oracle `STATICCALL`, it reads
//! the price straight out of a known storage slot on the aggregator (or any
//! contract), extracts the answer bit-field, and derives a [`Rate`] with the
//! same math as an ABI-decoded [`FeedConfig`](crate::FeedConfig). This is the
//! fast path — a single cold `SLOAD` per token, cacheable per block — and
//! keeps pricing consensus-consistent with the state the block is built on.
//!
//! A [`SlotFeed`] names the contract, the answer slot, and the [`SlotField`]
//! describing where the answer sits inside the 32-byte word (Solidity packs
//! multiple values right-aligned into one slot). An optional [`SlotTimestamp`]
//! points at the `updatedAt` word for staleness enforcement. The reader layer
//! performs the actual `SLOAD`s (see [`SlotFeed::answer_read`] /
//! [`SlotFeed::updated_at_read`]) and hands the raw words to
//! [`SlotFeed::reading`].

use alloy_primitives::{Address, U256};

use crate::{
    error::PricingError,
    feed::{FeedDirection, FeedReading},
    rate::Rate,
};

/// A right-aligned bit-field within a 32-byte storage word, matching Solidity's
/// packing of a struct member into a shared slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotField {
    /// Bit offset of the field's least-significant bit from the word's
    /// least-significant bit.
    pub bit_offset: u16,
    /// Field width in bits (`1..=256`).
    pub bit_len: u16,
    /// Whether the field is a two's-complement signed integer. A signed field
    /// whose sign bit is set is rejected as a non-positive answer.
    pub signed: bool,
}

impl SlotField {
    /// The entire word as an unsigned `uint256`.
    pub const WHOLE_WORD_UINT: Self = Self { bit_offset: 0, bit_len: 256, signed: false };
    /// The entire word as a signed `int256` (Chainlink-style answers).
    pub const WHOLE_WORD_INT: Self = Self { bit_offset: 0, bit_len: 256, signed: true };

    /// Extracts this field from `word`, returning the non-negative value.
    ///
    /// A signed field whose sign bit is set yields
    /// [`PricingError::NonPositiveAnswer`] — the payer never prices against a
    /// negative answer. A malformed field (zero width, wider than 256 bits, or
    /// extending past the word) yields [`PricingError::InvalidSlotField`].
    pub fn extract(&self, word: U256) -> Result<U256, PricingError> {
        let invalid = || PricingError::InvalidSlotField {
            bit_offset: self.bit_offset,
            bit_len: self.bit_len,
        };
        if self.bit_len == 0 || self.bit_len > 256 {
            return Err(invalid());
        }
        if u32::from(self.bit_offset) + u32::from(self.bit_len) > 256 {
            return Err(invalid());
        }

        let shifted = word >> usize::from(self.bit_offset);
        let value = if self.bit_len == 256 {
            shifted
        } else {
            let mask = (U256::from(1u8) << usize::from(self.bit_len)) - U256::from(1u8);
            shifted & mask
        };

        if self.signed && value.bit(usize::from(self.bit_len) - 1) {
            return Err(PricingError::NonPositiveAnswer);
        }
        Ok(value)
    }
}

/// A storage slot plus the [`SlotField`] carrying the feed's `updatedAt`
/// timestamp, used for staleness enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTimestamp {
    /// Storage slot holding the (packed) timestamp.
    pub slot: U256,
    /// Bit-field within that slot carrying the `updatedAt` value (unix seconds).
    pub field: SlotField,
}

/// A slot-read price source: a token price sourced by `SLOAD`-ing a known
/// storage slot rather than executing an oracle call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotFeed {
    /// Contract whose storage holds the price (e.g. a Chainlink aggregator).
    pub oracle: Address,
    /// Storage slot holding the (packed) answer.
    pub answer_slot: U256,
    /// Bit-field within the answer slot carrying the answer.
    pub answer_field: SlotField,
    /// Optional slot + field carrying `updatedAt` for staleness enforcement.
    pub updated_at: Option<SlotTimestamp>,
    /// Direction of the answer relative to the token and native asset.
    pub direction: FeedDirection,
    /// Decimal scaling of the answer (`answer = price × 10^answer_decimals`).
    pub answer_decimals: u8,
    /// ERC-20 decimals of the payment token.
    pub token_decimals: u8,
    /// Maximum permitted answer age in seconds; `0` disables the check (and
    /// requires [`Self::updated_at`] to be `None`).
    pub staleness_bound: u64,
}

impl SlotFeed {
    /// The `(contract, slot)` the reader must `SLOAD` for the answer word.
    pub const fn answer_read(&self) -> (Address, U256) {
        (self.oracle, self.answer_slot)
    }

    /// The `(contract, slot)` the reader must `SLOAD` for the timestamp word,
    /// when staleness is enforced.
    pub fn updated_at_read(&self) -> Option<(Address, U256)> {
        self.updated_at.map(|ts| (self.oracle, ts.slot))
    }

    /// Builds a [`FeedReading`] from the raw slot word(s) already `SLOAD`ed by
    /// the reader. `updated_at_word` must be supplied iff this feed carries an
    /// [`Self::updated_at`] location.
    pub fn reading(
        &self,
        answer_word: U256,
        updated_at_word: Option<U256>,
    ) -> Result<FeedReading, PricingError> {
        let answer = self.answer_field.extract(answer_word)?;
        let updated_at = match self.updated_at {
            None => None,
            Some(ts) => {
                let word = updated_at_word.ok_or(PricingError::MissingReading)?;
                let raw = ts.field.extract(word)?;
                if raw > U256::from(u64::MAX) {
                    return Err(PricingError::Overflow);
                }
                Some(raw.to::<u64>())
            }
        };
        Ok(FeedReading { answer, updated_at })
    }

    /// Resolves a [`FeedReading`] to an exact [`Rate`], enforcing positivity and
    /// the configured staleness bound.
    pub fn rate(&self, reading: FeedReading, now: u64) -> Result<Rate, PricingError> {
        let answer = reading.positive_answer()?;
        reading.ensure_fresh(self.staleness_bound, now)?;
        self.direction.rate(answer, self.answer_decimals, self.token_decimals)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const ORACLE: Address = address!("0x0000000000000000000000000000000000000abc");

    /// Packs a right-aligned value at `bit_offset` into a word.
    fn pack(value: U256, bit_offset: u16) -> U256 {
        value << usize::from(bit_offset)
    }

    #[test]
    fn whole_word_extract() {
        let word = U256::from(400_000_000_000_000u64);
        assert_eq!(SlotField::WHOLE_WORD_UINT.extract(word).unwrap(), word);
        assert_eq!(SlotField::WHOLE_WORD_INT.extract(word).unwrap(), word);
    }

    #[test]
    fn packed_field_extract() {
        // int192 answer at bits [0..192), uint64 timestamp at bits [192..256).
        let answer = U256::from(1_234_567u64);
        let timestamp = 1_700_000_000u64;
        let word = pack(answer, 0) | pack(U256::from(timestamp), 192);

        let answer_field = SlotField { bit_offset: 0, bit_len: 192, signed: true };
        let ts_field = SlotField { bit_offset: 192, bit_len: 64, signed: false };
        assert_eq!(answer_field.extract(word).unwrap(), answer);
        assert_eq!(ts_field.extract(word).unwrap(), U256::from(timestamp));
    }

    #[test]
    fn signed_negative_field_is_rejected() {
        // A 192-bit field with its sign bit (bit 191) set.
        let word = U256::from(1u8) << 191;
        let field = SlotField { bit_offset: 0, bit_len: 192, signed: true };
        assert_eq!(field.extract(word).unwrap_err(), PricingError::NonPositiveAnswer);
    }

    #[test]
    fn malformed_field_is_rejected() {
        let zero_len = SlotField { bit_offset: 0, bit_len: 0, signed: false };
        assert!(matches!(
            zero_len.extract(U256::ZERO).unwrap_err(),
            PricingError::InvalidSlotField { .. }
        ));
        let past_end = SlotField { bit_offset: 200, bit_len: 64, signed: false };
        assert!(matches!(
            past_end.extract(U256::ZERO).unwrap_err(),
            PricingError::InvalidSlotField { .. }
        ));
    }

    fn native_per_token_feed() -> SlotFeed {
        SlotFeed {
            oracle: ORACLE,
            answer_slot: U256::from(3u64),
            answer_field: SlotField::WHOLE_WORD_INT,
            updated_at: None,
            direction: FeedDirection::NativePerToken,
            answer_decimals: 18,
            token_decimals: 6,
            staleness_bound: 0,
        }
    }

    #[test]
    fn slot_feed_rate_matches_hand_computation() {
        let feed = native_per_token_feed();
        assert_eq!(feed.answer_read(), (ORACLE, U256::from(3u64)));
        assert!(feed.updated_at_read().is_none());

        // 1 token = 4e-4 native, answer_decimals 18 → same economics as the
        // ABI-decoded feed test: 1 ETH → 2500 token atomic (×1e6).
        let reading = feed.reading(U256::from(400_000_000_000_000u64), None).unwrap();
        let amount = feed.rate(reading, 0).unwrap().payment_amount(1_000_000_000, 1_000_000_000, 0);
        assert_eq!(amount.unwrap(), U256::from(2_500_000_000u64));
    }

    #[test]
    fn slot_feed_enforces_staleness() {
        let feed = SlotFeed {
            answer_slot: U256::from(3u64),
            // Answer and timestamp share slot 3: answer in low 192 bits.
            answer_field: SlotField { bit_offset: 0, bit_len: 192, signed: true },
            updated_at: Some(SlotTimestamp {
                slot: U256::from(3u64),
                field: SlotField { bit_offset: 192, bit_len: 64, signed: false },
            }),
            staleness_bound: 60,
            ..native_per_token_feed()
        };
        assert_eq!(feed.updated_at_read(), Some((ORACLE, U256::from(3u64))));

        // Answer and timestamp share slot 3: answer in low 192 bits.
        let answer = U256::from(400_000_000_000_000u64);
        let word = answer | (U256::from(100u64) << 192);
        let reading = feed.reading(word, Some(word)).unwrap();
        assert_eq!(reading.updated_at, Some(100));
        assert!(feed.rate(reading, 140).is_ok());
        assert_eq!(
            feed.rate(reading, 200).unwrap_err(),
            PricingError::StaleAnswer { age: 100, bound: 60 }
        );
    }

    #[test]
    fn missing_timestamp_word_is_rejected() {
        let feed = SlotFeed {
            updated_at: Some(SlotTimestamp {
                slot: U256::from(4u64),
                field: SlotField { bit_offset: 0, bit_len: 64, signed: false },
            }),
            staleness_bound: 60,
            ..native_per_token_feed()
        };
        assert_eq!(
            feed.reading(U256::from(1u64), None).unwrap_err(),
            PricingError::MissingReading
        );
    }
}
