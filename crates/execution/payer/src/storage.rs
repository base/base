//! Native read/write mirror of the on-chain payer-config system contract's
//! storage layout.
//!
//! [`PayerConfigStorage`] is the storage-layer dual of the pure [`PayerConfig`]
//! model: a reader (builder / node RPC) instantiates it over head/pending state
//! and calls [`PayerConfigStorage::read`] to materialize the whole config, or
//! [`PayerConfigStorage::read_token`] for a single accepted token. The setters
//! mirror the contract's admin mutations and back the round-trip tests.
//!
//! The layout keeps the enumerable accepted-token list in an
//! [`EnumerableSet`](base_precompile_storage::Set) and hangs each token's terms
//! off parallel mappings keyed by the token address. Everything a feed-backed
//! token needs beyond its oracle address (kind, margin, decimals, direction,
//! answer shape, selector, staleness bound) is packed into a single `terms`
//! word, decoded by [`TokenTerms`].

use alloy_primitives::{Address, U256, address};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Mapping, Result, Set};

use crate::{
    config::{PayerConfig, PriceSource, TokenConfig},
    feed::{AnswerShape, FeedConfig, FeedDirection},
    rate::Rate,
    slot::{SlotField, SlotFeed, SlotTimestamp},
    snapshot::{Erc20, PriceSnapshot, TokenPrice},
};

/// Read/write view over the on-chain payer-config system contract, mirroring
/// its storage layout under an ERC-7201 namespace:
///
/// ```solidity
/// address payer;                               // admin EOA co-signer / payee
/// bool    enabled;                             // service accepting txs
/// EnumerableSet.AddressSet tokens;             // accepted payment tokens
/// mapping(address => address) feeRecipient;    // phase-0 destination
/// mapping(address => uint256) terms;           // packed TokenTerms word
/// mapping(address => uint256) flatNumerator;   // flat kind only
/// mapping(address => uint256) flatDenominator; // flat kind only
/// mapping(address => address) oracle;          // feed / slot kinds
/// mapping(address => uint256) slotAnswerSlot;  // slot kind only
/// mapping(address => uint256) slotTimestampSlot; // slot kind, staleness only
/// mapping(address => uint256) slotFields;      // slot kind: packed bit-fields
/// ```
#[contract(addr = Self::ADDRESS)]
#[namespace("base.payer_config")]
pub struct PayerConfigStorage {
    /// Admin EOA that co-signs (`payer` / `payer_auth`) and receives payment.
    pub payer: Address,
    /// Whether the payer service is currently accepting transactions.
    pub enabled: bool,
    /// Enumerable set of accepted payment tokens.
    pub tokens: Set<Address>,
    /// Per-token phase-0 transfer destination (ERC-8168 `feeRecipient`).
    pub fee_recipient: Mapping<Address, Address>,
    /// Per-token packed [`TokenTerms`] word (kind, margin, feed parameters).
    pub terms: Mapping<Address, U256>,
    /// Per-token flat-rate numerator (populated for [`PriceSource::Flat`] only).
    pub flat_numerator: Mapping<Address, U256>,
    /// Per-token flat-rate denominator (populated for [`PriceSource::Flat`] only).
    pub flat_denominator: Mapping<Address, U256>,
    /// Per-token oracle contract (populated for [`PriceSource::Feed`] and
    /// [`PriceSource::Slot`]).
    pub oracle: Mapping<Address, Address>,
    /// Per-token answer storage slot (populated for [`PriceSource::Slot`] only).
    pub slot_answer_slot: Mapping<Address, U256>,
    /// Per-token `updatedAt` storage slot (populated for [`PriceSource::Slot`]
    /// with a staleness bound).
    pub slot_timestamp_slot: Mapping<Address, U256>,
    /// Per-token packed [`PackedSlotFields`] word (populated for
    /// [`PriceSource::Slot`] only).
    pub slot_fields: Mapping<Address, U256>,
}

impl PayerConfigStorage<'_> {
    /// Payer-config system-contract address, in the EIP-8130 system namespace
    /// (`0x8130…`) alongside the nonce manager (`…aa01`) and tx-context
    /// (`…aa02`) precompiles. Provisional; pinned when the reference contract
    /// is finalized.
    pub const ADDRESS: Address = address!("813000000000000000000000000000000000aa03");

    /// Materializes the full [`PayerConfig`] from storage, enumerating every
    /// accepted token and decoding its terms.
    pub fn read(&self) -> Result<PayerConfig> {
        let payer = self.payer.read()?;
        let enabled = self.enabled.read()?;
        let tokens = self.tokens.read()?;
        let mut configs = Vec::with_capacity(tokens.len());
        for token in &tokens {
            configs.push(self.read_token(*token)?);
        }
        Ok(PayerConfig { payer, enabled, tokens: configs })
    }

    /// Reads and decodes a single token's [`TokenConfig`]. The token is not
    /// required to be in the accepted set; an absent token reads back with an
    /// all-zero terms word, which decodes to a zero-margin flat `0/0` rate.
    pub fn read_token(&self, token: Address) -> Result<TokenConfig> {
        let fee_recipient = self.fee_recipient.at(&token).read()?;
        let terms = TokenTerms::from_word(self.terms.at(&token).read()?)?;
        let direction = || {
            FeedDirection::from_u8(terms.direction)
                .ok_or_else(BasePrecompileError::enum_conversion_error)
        };
        let price_source = match terms.kind {
            TokenTerms::KIND_FLAT => PriceSource::Flat(Rate::new(
                self.flat_numerator.at(&token).read()?,
                self.flat_denominator.at(&token).read()?,
            )),
            TokenTerms::KIND_FEED => PriceSource::Feed(FeedConfig {
                oracle: self.oracle.at(&token).read()?,
                selector: terms.selector,
                answer_shape: AnswerShape::from_u8(terms.shape)
                    .ok_or_else(BasePrecompileError::enum_conversion_error)?,
                direction: direction()?,
                answer_decimals: terms.answer_decimals,
                token_decimals: terms.token_decimals,
                staleness_bound: terms.staleness_bound,
            }),
            TokenTerms::KIND_SLOT => {
                let fields = PackedSlotFields::from_word(self.slot_fields.at(&token).read()?);
                let updated_at = fields.has_updated_at.then(|| {
                    Ok::<_, BasePrecompileError>(SlotTimestamp {
                        slot: self.slot_timestamp_slot.at(&token).read()?,
                        field: fields.timestamp_field,
                    })
                });
                PriceSource::Slot(SlotFeed {
                    oracle: self.oracle.at(&token).read()?,
                    answer_slot: self.slot_answer_slot.at(&token).read()?,
                    answer_field: fields.answer_field,
                    updated_at: updated_at.transpose()?,
                    direction: direction()?,
                    answer_decimals: terms.answer_decimals,
                    token_decimals: terms.token_decimals,
                    staleness_bound: terms.staleness_bound,
                })
            }
            _ => return Err(BasePrecompileError::enum_conversion_error()),
        };
        Ok(TokenConfig { token, fee_recipient, price_source, margin_bps: terms.margin_bps })
    }

    /// Sets the admin payer EOA.
    pub fn set_payer(&mut self, payer: Address) -> Result<()> {
        self.payer.write(payer)
    }

    /// Enables or disables the payer service.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        self.enabled.write(enabled)
    }

    /// Inserts or updates an accepted token's configuration, writing its terms
    /// word and the price-source-specific slots.
    pub fn upsert_token(&mut self, config: &TokenConfig) -> Result<()> {
        let token = config.token;
        self.tokens.insert(token)?;
        self.fee_recipient.at_mut(&token).write(config.fee_recipient)?;
        let terms = match &config.price_source {
            PriceSource::Flat(rate) => {
                self.flat_numerator.at_mut(&token).write(rate.numerator)?;
                self.flat_denominator.at_mut(&token).write(rate.denominator)?;
                TokenTerms::flat(config.margin_bps)
            }
            PriceSource::Feed(feed) => {
                self.oracle.at_mut(&token).write(feed.oracle)?;
                TokenTerms::feed(config.margin_bps, feed)
            }
            PriceSource::Slot(slot) => {
                self.oracle.at_mut(&token).write(slot.oracle)?;
                self.slot_answer_slot.at_mut(&token).write(slot.answer_slot)?;
                self.slot_fields.at_mut(&token).write(PackedSlotFields::of(slot).to_word())?;
                if let Some(ts) = slot.updated_at {
                    self.slot_timestamp_slot.at_mut(&token).write(ts.slot)?;
                }
                TokenTerms::slot(config.margin_bps, slot)
            }
        };
        self.terms.at_mut(&token).write(terms.to_word())
    }

    /// Resolves a chain-read-free [`PriceSnapshot`] at `now`, `SLOAD`-ing each
    /// slot-backed token's price against the current state.
    ///
    /// [`PriceSource::Flat`] prices are copied through. [`PriceSource::Slot`]
    /// prices are read from their configured slots; a token whose price cannot
    /// be resolved this block (stale/negative oracle answer, malformed field) is
    /// omitted from the snapshot rather than failing the whole read.
    /// [`PriceSource::Feed`] (`STATICCALL`) sources are not slot-resolvable and
    /// are likewise omitted — the builder fast path uses slot sources.
    ///
    /// Genuine state-access failures (a failed `SLOAD`) propagate.
    pub fn price_snapshot(&self, now: u64) -> Result<PriceSnapshot> {
        let config = self.read()?;
        let mut prices = Vec::with_capacity(config.tokens.len());
        for token in &config.tokens {
            let rate = match &token.price_source {
                PriceSource::Flat(rate) => *rate,
                PriceSource::Slot(feed) => {
                    let answer_word = self.storage.sload(feed.oracle, feed.answer_slot)?;
                    let timestamp_word = match feed.updated_at_read() {
                        Some((oracle, slot)) => Some(self.storage.sload(oracle, slot)?),
                        None => None,
                    };
                    match feed.reading(answer_word, timestamp_word).and_then(|r| feed.rate(r, now)) {
                        Ok(rate) => rate,
                        // Price unavailable this block (stale/negative/malformed).
                        Err(_) => continue,
                    }
                }
                // Not resolvable by SLOAD; excluded from the fast snapshot.
                PriceSource::Feed(_) => continue,
            };
            prices.push(TokenPrice {
                token: token.token,
                fee_recipient: token.fee_recipient,
                rate,
                margin_bps: token.margin_bps,
            });
        }
        Ok(PriceSnapshot { payer: config.payer, enabled: config.enabled, prices })
    }

    /// Reads `holder`'s balance of `token` with a single `SLOAD`, for a standard
    /// ERC-20 whose `balances` mapping is at `balances_base_slot`. The builder
    /// uses this to pre-screen payment before running the phase-0 transfer.
    pub fn token_balance(
        &self,
        token: Address,
        holder: Address,
        balances_base_slot: U256,
    ) -> Result<U256> {
        self.storage.sload(token, Erc20::balance_slot(holder, balances_base_slot))
    }

    /// Removes a token from the accepted set and clears its terms slots.
    pub fn remove_token(&mut self, token: Address) -> Result<bool> {
        let removed = self.tokens.remove(&token)?;
        if removed {
            self.fee_recipient.at_mut(&token).write(Address::ZERO)?;
            self.terms.at_mut(&token).write(U256::ZERO)?;
            self.flat_numerator.at_mut(&token).write(U256::ZERO)?;
            self.flat_denominator.at_mut(&token).write(U256::ZERO)?;
            self.oracle.at_mut(&token).write(Address::ZERO)?;
            self.slot_answer_slot.at_mut(&token).write(U256::ZERO)?;
            self.slot_timestamp_slot.at_mut(&token).write(U256::ZERO)?;
            self.slot_fields.at_mut(&token).write(U256::ZERO)?;
        }
        Ok(removed)
    }
}

/// Decoded per-token `terms` storage word.
///
/// Packed big-endian into a single slot, low bytes first, leaving the high 13
/// bytes reserved as zero:
///
/// ```text
/// byte 31    kind (0 = flat, 1 = feed)
/// bytes29-31 margin_bps (uint16)
/// byte 28    answer_decimals (feed)
/// byte 27    token_decimals  (feed)
/// byte 26    direction       (feed)
/// byte 25    answer shape    (feed)
/// bytes21-25 selector [4]    (feed)
/// bytes13-21 staleness_bound (uint64, feed)
/// ```
struct TokenTerms {
    kind: u8,
    margin_bps: u16,
    answer_decimals: u8,
    token_decimals: u8,
    direction: u8,
    shape: u8,
    selector: [u8; 4],
    staleness_bound: u64,
}

impl TokenTerms {
    const KIND_FLAT: u8 = 0;
    const KIND_FEED: u8 = 1;
    const KIND_SLOT: u8 = 2;

    /// Terms for a flat-rate token (feed fields left zero).
    const fn flat(margin_bps: u16) -> Self {
        Self {
            kind: Self::KIND_FLAT,
            margin_bps,
            answer_decimals: 0,
            token_decimals: 0,
            direction: 0,
            shape: 0,
            selector: [0; 4],
            staleness_bound: 0,
        }
    }

    /// Terms for a `STATICCALL` feed-backed token, projecting its parameters.
    const fn feed(margin_bps: u16, feed: &FeedConfig) -> Self {
        Self {
            kind: Self::KIND_FEED,
            margin_bps,
            answer_decimals: feed.answer_decimals,
            token_decimals: feed.token_decimals,
            direction: feed.direction.to_u8(),
            shape: feed.answer_shape.to_u8(),
            selector: feed.selector,
            staleness_bound: feed.staleness_bound,
        }
    }

    /// Terms for a slot-read token, projecting its scalar parameters. The
    /// bit-field layout lives in the companion [`PackedSlotFields`] word.
    const fn slot(margin_bps: u16, slot: &SlotFeed) -> Self {
        Self {
            kind: Self::KIND_SLOT,
            margin_bps,
            answer_decimals: slot.answer_decimals,
            token_decimals: slot.token_decimals,
            direction: slot.direction.to_u8(),
            shape: 0,
            selector: [0; 4],
            staleness_bound: slot.staleness_bound,
        }
    }

    /// Packs these terms into their storage word — the exact inverse of
    /// [`Self::from_word`].
    fn to_word(&self) -> U256 {
        let mut b = [0u8; 32];
        b[31] = self.kind;
        b[29..31].copy_from_slice(&self.margin_bps.to_be_bytes());
        b[28] = self.answer_decimals;
        b[27] = self.token_decimals;
        b[26] = self.direction;
        b[25] = self.shape;
        b[21..25].copy_from_slice(&self.selector);
        b[13..21].copy_from_slice(&self.staleness_bound.to_be_bytes());
        U256::from_be_bytes(b)
    }

    /// Unpacks a raw `terms` storage word, rejecting an unknown `kind`.
    fn from_word(word: U256) -> Result<Self> {
        let b = word.to_be_bytes::<32>();
        let kind = b[31];
        if kind != Self::KIND_FLAT && kind != Self::KIND_FEED && kind != Self::KIND_SLOT {
            return Err(BasePrecompileError::enum_conversion_error());
        }
        Ok(Self {
            kind,
            margin_bps: u16::from_be_bytes([b[29], b[30]]),
            answer_decimals: b[28],
            token_decimals: b[27],
            direction: b[26],
            shape: b[25],
            selector: [b[21], b[22], b[23], b[24]],
            staleness_bound: u64::from_be_bytes(
                b[13..21].try_into().expect("8-byte slice is a valid u64"),
            ),
        })
    }
}

/// Decoded per-token `slot_fields` storage word: the [`SlotField`] bit-field
/// layouts for a [`PriceSource::Slot`] source's answer and (optional)
/// timestamp, packed into one word alongside the has-timestamp flag.
///
/// ```text
/// byte 31    has_updated_at (0 / 1)
/// byte 30    answer field signed
/// bytes28-30 answer field bit_offset (uint16)
/// bytes26-28 answer field bit_len    (uint16)
/// byte 25    timestamp field signed
/// bytes23-25 timestamp field bit_offset (uint16)
/// bytes21-23 timestamp field bit_len    (uint16)
/// ```
struct PackedSlotFields {
    answer_field: SlotField,
    has_updated_at: bool,
    timestamp_field: SlotField,
}

impl PackedSlotFields {
    /// Projects the bit-field layout of a [`SlotFeed`].
    const fn of(feed: &SlotFeed) -> Self {
        match feed.updated_at {
            Some(ts) => Self {
                answer_field: feed.answer_field,
                has_updated_at: true,
                timestamp_field: ts.field,
            },
            None => Self {
                answer_field: feed.answer_field,
                has_updated_at: false,
                timestamp_field: SlotField { bit_offset: 0, bit_len: 0, signed: false },
            },
        }
    }

    /// Packs the layout into its storage word — the exact inverse of
    /// [`Self::from_word`].
    fn to_word(&self) -> U256 {
        let mut b = [0u8; 32];
        b[31] = u8::from(self.has_updated_at);
        b[30] = u8::from(self.answer_field.signed);
        b[28..30].copy_from_slice(&self.answer_field.bit_offset.to_be_bytes());
        b[26..28].copy_from_slice(&self.answer_field.bit_len.to_be_bytes());
        b[25] = u8::from(self.timestamp_field.signed);
        b[23..25].copy_from_slice(&self.timestamp_field.bit_offset.to_be_bytes());
        b[21..23].copy_from_slice(&self.timestamp_field.bit_len.to_be_bytes());
        U256::from_be_bytes(b)
    }

    /// Unpacks a raw `slot_fields` storage word.
    const fn from_word(word: U256) -> Self {
        let b = word.to_be_bytes::<32>();
        Self {
            answer_field: SlotField {
                bit_offset: u16::from_be_bytes([b[28], b[29]]),
                bit_len: u16::from_be_bytes([b[26], b[27]]),
                signed: b[30] != 0,
            },
            has_updated_at: b[31] != 0,
            timestamp_field: SlotField {
                bit_offset: u16::from_be_bytes([b[23], b[24]]),
                bit_len: u16::from_be_bytes([b[21], b[22]]),
                signed: b[25] != 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;

    const PAYER: Address = address!("0x0000000000000000000000000000000000000099");
    const TOKEN_A: Address = address!("0x0000000000000000000000000000000000000011");
    const TOKEN_B: Address = address!("0x0000000000000000000000000000000000000022");
    const ORACLE: Address = address!("0x0000000000000000000000000000000000000abc");
    /// Slot the slot-backed test token reads its packed answer/timestamp from.
    const ANSWER_SLOT: U256 = U256::from_limbs([3, 0, 0, 0]);

    /// Packs a `native_per_token` answer (`4e14`, low 192 bits) and `updated_at`
    /// (high 64 bits) into the shared answer slot word.
    fn slot_word(updated_at: u64) -> U256 {
        U256::from(400_000_000_000_000u64) | (U256::from(updated_at) << 192)
    }

    fn flat_token(token: Address) -> TokenConfig {
        TokenConfig {
            token,
            fee_recipient: address!("0x00000000000000000000000000000000000000fe"),
            price_source: PriceSource::Flat(Rate::new(U256::from(1u64), U256::from(400_000_000u64))),
            margin_bps: 250,
        }
    }

    fn feed_token(token: Address) -> TokenConfig {
        TokenConfig {
            token,
            fee_recipient: address!("0x00000000000000000000000000000000000000fd"),
            price_source: PriceSource::Feed(FeedConfig {
                oracle: address!("0x0000000000000000000000000000000000000abc"),
                selector: [0xfe, 0xaf, 0x96, 0x8c],
                answer_shape: AnswerShape::ChainlinkRoundData,
                direction: FeedDirection::TokenPerNative,
                answer_decimals: 8,
                token_decimals: 6,
                staleness_bound: 3600,
            }),
            margin_bps: 100,
        }
    }

    #[test]
    fn flat_token_round_trips() {
        let mut storage = HashMapStorageProvider::new(1);
        let cfg = flat_token(TOKEN_A);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.set_payer(PAYER).unwrap();
            s.set_enabled(true).unwrap();
            s.upsert_token(&cfg).unwrap();

            let read = s.read().unwrap();
            assert_eq!(read.payer, PAYER);
            assert!(read.enabled);
            assert_eq!(read.tokens.as_slice(), &[cfg]);
        });
    }

    fn slot_token(token: Address, staleness_bound: u64) -> TokenConfig {
        let updated_at = (staleness_bound > 0).then_some(SlotTimestamp {
            slot: ANSWER_SLOT,
            field: SlotField { bit_offset: 192, bit_len: 64, signed: false },
        });
        TokenConfig {
            token,
            fee_recipient: address!("0x00000000000000000000000000000000000000fc"),
            price_source: PriceSource::Slot(SlotFeed {
                oracle: ORACLE,
                answer_slot: ANSWER_SLOT,
                answer_field: SlotField { bit_offset: 0, bit_len: 192, signed: true },
                updated_at,
                direction: FeedDirection::NativePerToken,
                answer_decimals: 18,
                token_decimals: 6,
                staleness_bound,
            }),
            margin_bps: 50,
        }
    }

    #[test]
    fn feed_token_round_trips() {
        let mut storage = HashMapStorageProvider::new(1);
        let cfg = feed_token(TOKEN_A);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.upsert_token(&cfg).unwrap();
            assert_eq!(s.read_token(TOKEN_A).unwrap(), cfg);
        });
    }

    #[test]
    fn slot_token_round_trips() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            // With and without a staleness timestamp location.
            let with_ts = slot_token(TOKEN_A, 3600);
            let without_ts = slot_token(TOKEN_B, 0);
            s.upsert_token(&with_ts).unwrap();
            s.upsert_token(&without_ts).unwrap();
            assert_eq!(s.read_token(TOKEN_A).unwrap(), with_ts);
            assert_eq!(s.read_token(TOKEN_B).unwrap(), without_ts);
        });
    }

    #[test]
    fn multiple_tokens_are_enumerable() {
        let mut storage = HashMapStorageProvider::new(1);
        let (a, b) = (flat_token(TOKEN_A), feed_token(TOKEN_B));
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.upsert_token(&a).unwrap();
            s.upsert_token(&b).unwrap();

            let read = s.read().unwrap();
            assert_eq!(read.tokens.len(), 2);
            assert_eq!(read.token(TOKEN_A), Some(&a));
            assert_eq!(read.token(TOKEN_B), Some(&b));
        });
    }

    #[test]
    fn upsert_overwrites_existing_terms() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.upsert_token(&flat_token(TOKEN_A)).unwrap();
            // Re-key the same token as a feed source; the set stays deduplicated.
            let updated = feed_token(TOKEN_A);
            s.upsert_token(&updated).unwrap();

            let read = s.read().unwrap();
            assert_eq!(read.tokens.len(), 1);
            assert_eq!(read.token(TOKEN_A), Some(&updated));
        });
    }

    #[test]
    fn remove_token_clears_it() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.upsert_token(&flat_token(TOKEN_A)).unwrap();
            assert!(s.remove_token(TOKEN_A).unwrap());
            assert!(!s.remove_token(TOKEN_A).unwrap());
            assert!(s.read().unwrap().tokens.is_empty());
        });
    }

    #[test]
    fn unknown_terms_kind_is_rejected() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            // Write a terms word with an out-of-range kind byte directly.
            s.terms.at_mut(&TOKEN_A).write(U256::from(0xffu64)).unwrap();
            assert!(s.read_token(TOKEN_A).is_err());
        });
    }

    #[test]
    fn price_snapshot_resolves_flat_and_slot() {
        let mut storage = HashMapStorageProvider::new(1);
        let now = 1_000u64;
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.set_payer(PAYER).unwrap();
            s.set_enabled(true).unwrap();
            s.upsert_token(&flat_token(TOKEN_A)).unwrap();
            s.upsert_token(&slot_token(TOKEN_B, 3600)).unwrap();
            // Seed the oracle answer slot with a fresh reading.
            ctx.sstore(ORACLE, ANSWER_SLOT, slot_word(now)).unwrap();

            let snapshot = s.price_snapshot(now).unwrap();
            assert_eq!(snapshot.payer, PAYER);
            assert!(snapshot.enabled);
            assert_eq!(snapshot.prices.len(), 2);
            // Both sources encode the same 2.5e9 base (1 ETH → 2500 token atomic
            // ×1e6 for a 1e18-wei gas cost); the amounts differ only by each
            // token's folded margin (flat 250bps, slot 50bps).
            let flat = snapshot.token(TOKEN_A).unwrap();
            let slot = snapshot.token(TOKEN_B).unwrap();
            assert_eq!(
                flat.payment_amount(1_000_000_000, 1_000_000_000).unwrap(),
                U256::from(2_562_500_000u64)
            );
            assert_eq!(
                slot.payment_amount(1_000_000_000, 1_000_000_000).unwrap(),
                U256::from(2_512_500_000u64)
            );
        });
    }

    #[test]
    fn price_snapshot_omits_stale_slot_token() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut s = PayerConfigStorage::new(ctx);
            s.upsert_token(&slot_token(TOKEN_B, 3600)).unwrap();
            // updated_at=100 with now far beyond the 3600s bound → unavailable.
            ctx.sstore(ORACLE, ANSWER_SLOT, slot_word(100)).unwrap();

            let snapshot = s.price_snapshot(100_000).unwrap();
            assert!(snapshot.prices.is_empty());
            assert!(snapshot.token(TOKEN_B).is_none());
        });
    }

    #[test]
    fn token_balance_reads_mapping_slot() {
        let mut storage = HashMapStorageProvider::new(1);
        let holder = address!("0x00000000000000000000000000000000000000aa");
        let base = U256::from(9u64);
        StorageCtx::enter(&mut storage, |ctx| {
            ctx.sstore(TOKEN_A, Erc20::balance_slot(holder, base), U256::from(1_234u64)).unwrap();
            let balance = PayerConfigStorage::new(ctx).token_balance(TOKEN_A, holder, base).unwrap();
            assert_eq!(balance, U256::from(1_234u64));
        });
    }
}
