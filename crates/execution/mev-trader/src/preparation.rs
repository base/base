use std::{collections::BTreeMap, time::Instant};

use alloy_primitives::{Address, I256, U256};
use thiserror::Error;

use crate::{
    AuditedWriteKey, CancellationProbe, ExactProtocol, FEE_DENOMINATOR, FieldRead, MAX_V3_TICK,
    MIN_V3_TICK, MaterializedState, PairwiseV3Tick, PoolUniverseSnapshot, PreparedPoolQuote,
    PreparedPoolState, StoragePlanValidator, StorageReadPlan, V3PreparedState, V3StorageValidator,
};

/// Fail-closed errors produced while decoding one complete materialized universe.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PreparationError {
    /// A required audited materialized value is absent.
    #[error("required materialized write is missing")]
    MissingWrite,
    /// Materialized writes duplicate or conflict at one logical key.
    #[error("materialized write conflicts with audit authority")]
    ConflictingWrite,
    /// A decoded field cannot be represented by its required type.
    #[error("decoded field is out of range")]
    FieldOutOfRange,
    /// An Aerodrome stable descriptor does not decode an exact enabled stable flag.
    #[error("stable flag does not equal one")]
    StableFlagMismatch,
    /// Uniswap V3 bitmap, sentinel, or initialized-tick coverage is incomplete.
    #[error("uniswap v3 coverage is incomplete")]
    V3Coverage,
    /// A decoded prepared pool fails pairwise input validation.
    #[error("prepared pool is invalid")]
    InvalidPreparedPool,
    /// The bounded output or materialized index exceeds its approved limit.
    #[error("preparation limit exceeded")]
    LimitExceeded,
    /// Cooperative cancellation stopped preparation without partial output.
    #[error("preparation cancelled")]
    Cancelled,
}

/// Provider-free decoder from audited materialized writes to complete pairwise pool states.
#[derive(Debug, Default, Clone, Copy)]
pub struct PoolStatePreparer;

impl PoolStatePreparer {
    /// Decodes every pool in snapshot order or returns an error without partial output.
    pub fn prepare(
        universe: &PoolUniverseSnapshot,
        materialized: &MaterializedState,
        cancellation: &CancellationProbe,
    ) -> Result<Vec<PreparedPoolState>, PreparationError> {
        Self::checkpoint(cancellation)?;
        let values = Self::index(universe, materialized, cancellation)?;
        let mut prepared = Vec::with_capacity(universe.descriptors().len());
        for descriptor in universe.descriptors() {
            Self::checkpoint(cancellation)?;
            let quote = match (&descriptor.protocol, &descriptor.read_plan) {
                (
                    ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile,
                    StorageReadPlan::ConstantProduct { reserve0, reserve1 },
                ) => PreparedPoolQuote::constant_product(
                    Self::decode_unsigned(descriptor.pool, *reserve0, &values)?,
                    Self::decode_unsigned(descriptor.pool, *reserve1, &values)?,
                ),
                (
                    ExactProtocol::AerodromeStable,
                    StorageReadPlan::Stable { reserve0, reserve1, stable },
                ) => {
                    if Self::decode_unsigned(descriptor.pool, *stable, &values)? != U256::from(1) {
                        return Err(PreparationError::StableFlagMismatch);
                    }
                    PreparedPoolQuote::stable(
                        Self::decode_unsigned(descriptor.pool, *reserve0, &values)?,
                        Self::decode_unsigned(descriptor.pool, *reserve1, &values)?,
                    )
                }
                (
                    ExactProtocol::UniswapV3,
                    plan @ StorageReadPlan::V3 {
                        sqrt_price_x96,
                        liquidity,
                        current_tick,
                        tick_spacing,
                        lower_word,
                        upper_word,
                        words,
                        lower_sentinel,
                        upper_sentinel,
                        initialized_ticks,
                        ..
                    },
                ) => {
                    let full_bounds = Self::full_v3_word_bounds(*tick_spacing)?;
                    if (*lower_word, *upper_word) != full_bounds {
                        return Err(PreparationError::V3Coverage);
                    }
                    if descriptor.fee >= FEE_DENOMINATOR {
                        return Err(PreparationError::FieldOutOfRange);
                    }
                    let mut prepared_words = Vec::with_capacity(words.len());
                    for word in words {
                        Self::checkpoint(cancellation)?;
                        prepared_words.push((
                            word.word_position,
                            Self::storage_value(descriptor.pool, word.slot, &values)?,
                        ));
                    }
                    let v3 = V3PreparedState {
                        words: prepared_words,
                        lower_sentinel: Self::storage_value(
                            descriptor.pool,
                            lower_sentinel.slot,
                            &values,
                        )?,
                        upper_sentinel: Self::storage_value(
                            descriptor.pool,
                            upper_sentinel.slot,
                            &values,
                        )?,
                        initialized_ticks: initialized_ticks.clone(),
                    };
                    if V3StorageValidator::validate_prepared(plan, &v3, cancellation).is_err() {
                        Self::checkpoint(cancellation)?;
                        return Err(PreparationError::V3Coverage);
                    }
                    let mut ticks = Vec::with_capacity(initialized_ticks.len());
                    for initialized in initialized_ticks {
                        Self::checkpoint(cancellation)?;
                        let gross = Self::decode_unsigned(
                            descriptor.pool,
                            initialized.liquidity_gross,
                            &values,
                        )?;
                        if gross.is_zero() || u128::try_from(gross).is_err() {
                            return Err(PreparationError::V3Coverage);
                        }
                        ticks.push(PairwiseV3Tick {
                            tick: initialized.tick,
                            liquidity_net: Self::decode_signed(
                                descriptor.pool,
                                initialized.liquidity_net,
                                &values,
                            )?,
                        });
                    }
                    let tick = Self::decode_signed(descriptor.pool, *current_tick, &values)?
                        .try_into()
                        .map_err(|_| PreparationError::FieldOutOfRange)?;
                    PreparedPoolQuote::v3(
                        Self::decode_unsigned(descriptor.pool, *sqrt_price_x96, &values)?,
                        Self::decode_unsigned(descriptor.pool, *liquidity, &values)?,
                        tick,
                        *tick_spacing,
                        ticks,
                    )
                }
                _ => return Err(PreparationError::InvalidPreparedPool),
            };
            if descriptor.fee > FEE_DENOMINATOR {
                return Err(PreparationError::FieldOutOfRange);
            }
            let state = PreparedPoolState {
                pool: descriptor.pool,
                protocol: descriptor.protocol,
                token0: descriptor.token0,
                token1: descriptor.token1,
                decimals0: descriptor.decimals0,
                decimals1: descriptor.decimals1,
                fee_pips: descriptor.fee,
                quote,
            };
            state.validate().map_err(|_| PreparationError::InvalidPreparedPool)?;
            prepared.push(state);
        }
        Self::checkpoint(cancellation)?;
        Ok(prepared)
    }

    /// Builds the exact logical materialized-write index required by the snapshot audit plan.
    pub fn index(
        universe: &PoolUniverseSnapshot,
        materialized: &MaterializedState,
        cancellation: &CancellationProbe,
    ) -> Result<BTreeMap<(u8, Address, U256), U256>, PreparationError> {
        let audit = universe.audit().audited_writes();
        let expected: BTreeMap<_, _> =
            audit.iter().map(|key| (Self::logical_key(key), *key)).collect();
        let mut values = BTreeMap::new();
        for write in &materialized.writes {
            Self::checkpoint(cancellation)?;
            let logical = Self::logical_key(&write.key);
            if expected.get(&logical) != Some(&write.key)
                || values.insert(logical, write.value).is_some()
            {
                return Err(PreparationError::ConflictingWrite);
            }
        }
        if values.len() != expected.len() {
            let missing_v3_coverage = expected.keys().any(|logical| {
                !values.contains_key(logical)
                    && Self::is_v3_coverage_slot(universe, logical.1, logical.2)
            });
            return Err(if missing_v3_coverage {
                PreparationError::V3Coverage
            } else {
                PreparationError::MissingWrite
            });
        }
        Ok(values)
    }

    /// Returns whether an exact storage key belongs to fixed V3 bitmap or tick coverage.
    pub fn is_v3_coverage_slot(universe: &PoolUniverseSnapshot, pool: Address, slot: U256) -> bool {
        universe.descriptors().iter().any(|descriptor| {
            if descriptor.pool != pool {
                return false;
            }
            let StorageReadPlan::V3 {
                words,
                lower_sentinel,
                upper_sentinel,
                initialized_ticks,
                ..
            } = &descriptor.read_plan
            else {
                return false;
            };
            words.iter().any(|word| word.slot == slot)
                || lower_sentinel.slot == slot
                || upper_sentinel.slot == slot
                || initialized_ticks.iter().any(|tick| {
                    tick.liquidity_gross.slot == slot || tick.liquidity_net.slot == slot
                })
        })
    }

    /// Returns the evidence-independent identity used by materialization and delta authorization.
    pub const fn logical_key(key: &AuditedWriteKey) -> (u8, Address, U256) {
        match key {
            AuditedWriteKey::AccountBalance { address, .. } => (0, *address, U256::ZERO),
            AuditedWriteKey::AccountNonce { address, .. } => (1, *address, U256::ZERO),
            AuditedWriteKey::Storage { address, slot, .. } => (2, *address, *slot),
        }
    }

    /// Returns the complete legal V3 bitmap-word domain for one tick spacing.
    pub fn full_v3_word_bounds(tick_spacing: i32) -> Result<(i16, i16), PreparationError> {
        if tick_spacing <= 0 {
            return Err(PreparationError::V3Coverage);
        }
        let minimum_remainder = MIN_V3_TICK.rem_euclid(tick_spacing);
        let minimum_aligned = if minimum_remainder == 0 {
            MIN_V3_TICK
        } else {
            MIN_V3_TICK
                .checked_add(tick_spacing - minimum_remainder)
                .ok_or(PreparationError::V3Coverage)?
        };
        let maximum_aligned = MAX_V3_TICK
            .checked_sub(MAX_V3_TICK.rem_euclid(tick_spacing))
            .ok_or(PreparationError::V3Coverage)?;
        let (lower_word, _) =
            V3StorageValidator::compressed_position(minimum_aligned, tick_spacing)
                .map_err(|_| PreparationError::V3Coverage)?;
        let (upper_word, _) =
            V3StorageValidator::compressed_position(maximum_aligned, tick_spacing)
                .map_err(|_| PreparationError::V3Coverage)?;
        Ok((
            i16::try_from(lower_word).map_err(|_| PreparationError::V3Coverage)?,
            i16::try_from(upper_word).map_err(|_| PreparationError::V3Coverage)?,
        ))
    }

    /// Decodes one unsigned bit field after revalidating its exact range.
    pub fn decode_unsigned(
        pool: Address,
        field: FieldRead,
        values: &BTreeMap<(u8, Address, U256), U256>,
    ) -> Result<U256, PreparationError> {
        if field.signed {
            return Err(PreparationError::FieldOutOfRange);
        }
        let raw = Self::storage_value(pool, field.slot, values)?;
        Self::extract(raw, field)
    }

    /// Decodes one signed two's-complement bit field with exact sign extension.
    pub fn decode_signed(
        pool: Address,
        field: FieldRead,
        values: &BTreeMap<(u8, Address, U256), U256>,
    ) -> Result<I256, PreparationError> {
        if !field.signed {
            return Err(PreparationError::FieldOutOfRange);
        }
        let value = Self::extract(Self::storage_value(pool, field.slot, values)?, field)?;
        let sign_bit = U256::from(1) << usize::from(field.bit_width - 1);
        let extended = if value & sign_bit == U256::ZERO || field.bit_width == 256 {
            value
        } else {
            value | (U256::MAX << usize::from(field.bit_width))
        };
        Ok(I256::from_raw(extended))
    }

    /// Extracts a structurally valid field from one raw storage word.
    pub fn extract(raw: U256, field: FieldRead) -> Result<U256, PreparationError> {
        StoragePlanValidator::validate_field(field, field.kind, field.signed)
            .map_err(|_| PreparationError::FieldOutOfRange)?;
        let shifted = raw >> usize::from(field.bit_offset);
        let mask = if field.bit_width == 256 {
            U256::MAX
        } else {
            (U256::from(1) << usize::from(field.bit_width)) - U256::from(1)
        };
        Ok(shifted & mask)
    }

    /// Reads one exact pool storage value from the verified materialized index.
    pub fn storage_value(
        pool: Address,
        slot: U256,
        values: &BTreeMap<(u8, Address, U256), U256>,
    ) -> Result<U256, PreparationError> {
        values.get(&(2, pool, slot)).copied().ok_or(PreparationError::MissingWrite)
    }

    /// Performs one cooperative cancellation checkpoint.
    pub fn checkpoint(cancellation: &CancellationProbe) -> Result<(), PreparationError> {
        if cancellation.checkpoint(Instant::now(), true) {
            Ok(())
        } else {
            cancellation.acknowledge_drop();
            Err(PreparationError::Cancelled)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::B256;

    use super::*;
    use crate::{
        BitmapWordRead, CancellationToken, CoverageHasher, DescriptorHasher, DescriptorPlanDigest,
        FieldKind, FixturePoolRegistry, GlobalLifecycle, InitializedTickRead, MaterializedWrite,
        PoolDescriptor, RegistryHasher,
    };

    fn probe() -> CancellationProbe {
        CancellationProbe::new(
            Arc::new(CancellationToken::new(Instant::now() + Duration::from_secs(5))),
            Arc::new(GlobalLifecycle::default()),
        )
    }

    fn field(
        kind: FieldKind,
        slot: u64,
        bit_offset: u16,
        bit_width: u16,
        signed: bool,
    ) -> FieldRead {
        FieldRead { kind, slot: U256::from(slot), bit_offset, bit_width, signed }
    }

    fn descriptor(
        pool_byte: u8,
        protocol: ExactProtocol,
        read_plan: StorageReadPlan,
        fee: u32,
    ) -> PoolDescriptor {
        let pool = Address::with_last_byte(pool_byte);
        let mut audited_writes: Vec<_> = read_plan
            .storage_slots()
            .into_iter()
            .map(|slot| AuditedWriteKey::Storage {
                address: pool,
                slot,
                evidence_digest: B256::with_last_byte(
                    pool_byte.wrapping_add(slot.as_limbs()[0] as u8).wrapping_add(1).max(1),
                ),
            })
            .collect();
        audited_writes.sort_unstable();
        let mut descriptor = PoolDescriptor {
            pool,
            protocol,
            token0: Address::with_last_byte(1),
            token1: Address::with_last_byte(2),
            decimals0: 6,
            decimals1: 18,
            fee,
            code_hash: B256::with_last_byte(3),
            read_plan,
            audited_writes,
            descriptor_digest: DescriptorPlanDigest(B256::ZERO),
        };
        descriptor.descriptor_digest =
            DescriptorHasher::digest(&descriptor).expect("descriptor digest");
        descriptor
    }

    fn universe(descriptors: Vec<PoolDescriptor>) -> PoolUniverseSnapshot {
        let digest = RegistryHasher::digest(&descriptors).expect("registry digest");
        let registry = FixturePoolRegistry::new(descriptors, digest).expect("registry");
        PoolUniverseSnapshot::capture(&registry).expect("snapshot")
    }

    fn materialized(
        universe: &PoolUniverseSnapshot,
        value: impl Fn(Address, U256) -> U256,
    ) -> MaterializedState {
        MaterializedState {
            writes: universe
                .audit()
                .audited_writes()
                .iter()
                .map(|key| MaterializedWrite {
                    key: *key,
                    value: value(key.address(), key.slot().unwrap_or(U256::ZERO)),
                })
                .collect(),
        }
    }

    #[test]
    fn t4a_preparer_decodes_univ2_aero_volatile_and_stable_exactly() {
        let packed = StorageReadPlan::ConstantProduct {
            reserve0: field(FieldKind::Reserve0, 0, 0, 112, false),
            reserve1: field(FieldKind::Reserve1, 0, 112, 112, false),
        };
        let stable = StorageReadPlan::Stable {
            reserve0: field(FieldKind::Reserve0, 0, 0, 112, false),
            reserve1: field(FieldKind::Reserve1, 0, 112, 112, false),
            stable: field(FieldKind::StableFlag, 1, 0, 1, false),
        };
        let snapshot = universe(vec![
            descriptor(10, ExactProtocol::UniswapV2, packed.clone(), 3_000),
            descriptor(11, ExactProtocol::AerodromeVolatile, packed, 2_000),
            descriptor(12, ExactProtocol::AerodromeStable, stable, 1_000),
        ]);
        let reserve0 = U256::from(123);
        let reserve1 = U256::from(456);
        let packed_value = reserve0 | (reserve1 << 112);
        let state =
            materialized(
                &snapshot,
                |_, slot| {
                    if slot == U256::ZERO { packed_value } else { U256::from(1) }
                },
            );
        let prepared =
            PoolStatePreparer::prepare(&snapshot, &state, &probe()).expect("prepared universe");
        assert_eq!(prepared[0].quote, PreparedPoolQuote::constant_product(reserve0, reserve1));
        assert_eq!(prepared[1].quote, PreparedPoolQuote::constant_product(reserve0, reserve1));
        assert_eq!(prepared[2].quote, PreparedPoolQuote::stable(reserve0, reserve1));
        assert_eq!(
            (prepared[0].decimals0, prepared[0].decimals1, prepared[0].fee_pips),
            (6, 18, 3_000)
        );
        assert_eq!(prepared[1].fee_pips, 2_000);
        assert_eq!(prepared[2].fee_pips, 1_000);

        let mut disabled = state.clone();
        let stable_flag = disabled
            .writes
            .iter()
            .position(|write| {
                write.key.address() == Address::with_last_byte(12)
                    && write.key.slot() == Some(U256::from(1))
            })
            .expect("stable flag");
        disabled.writes[stable_flag].value = U256::ZERO;
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &disabled, &probe()),
            Err(PreparationError::StableFlagMismatch)
        );
        disabled.writes[stable_flag].value = U256::from(2);
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &disabled, &probe()),
            Err(PreparationError::StableFlagMismatch)
        );

        let mut missing = state;
        missing.writes.pop();
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &missing, &probe()),
            Err(PreparationError::MissingWrite)
        );
    }

    #[test]
    fn t4a_preparer_decodes_v3_ticks_and_rejects_incomplete_coverage() {
        let tick_spacing = 1;
        let (lower_word, upper_word) =
            PoolStatePreparer::full_v3_word_bounds(tick_spacing).expect("full V3 bounds");
        assert_eq!((lower_word, upper_word), (-3_466, 3_465));
        assert!(
            usize::try_from(i32::from(upper_word) - i32::from(lower_word) + 1).expect("word count")
                <= crate::MAX_V3_BITMAP_WORDS
        );
        let bitmap_slot = 3_476u64;
        let lower_sentinel_slot = 7_000u64;
        let upper_sentinel_slot = 7_001u64;
        let liquidity_gross_slot = 7_002u64;
        let liquidity_net_slot = 7_003u64;

        let initialized = InitializedTickRead {
            tick: 0,
            liquidity_gross: field(FieldKind::LiquidityGross, liquidity_gross_slot, 0, 128, false),
            liquidity_net: field(FieldKind::LiquidityNet, liquidity_net_slot, 0, 128, true),
        };
        let mut plan = StorageReadPlan::V3 {
            sqrt_price_x96: field(FieldKind::SqrtPriceX96, 1, 0, 160, false),
            liquidity: field(FieldKind::Liquidity, 2, 0, 128, false),
            current_tick: field(FieldKind::CurrentTick, 3, 0, 24, true),
            tick_spacing,
            lower_word,
            upper_word,
            words: (lower_word..=upper_word)
                .enumerate()
                .map(|(offset, word_position)| BitmapWordRead {
                    word_position,
                    slot: U256::from(10 + offset),
                })
                .collect(),
            lower_sentinel: BitmapWordRead {
                word_position: lower_word - 1,
                slot: U256::from(lower_sentinel_slot),
            },
            upper_sentinel: BitmapWordRead {
                word_position: upper_word + 1,
                slot: U256::from(upper_sentinel_slot),
            },
            initialized_ticks: vec![initialized],
            coverage_digest: B256::ZERO,
        };
        let coverage_digest = CoverageHasher::digest(&plan).expect("coverage digest");
        if let StorageReadPlan::V3 { coverage_digest: digest, .. } = &mut plan {
            *digest = coverage_digest;
        }

        let mut unaligned = plan.clone();
        if let StorageReadPlan::V3 { initialized_ticks, .. } = &mut unaligned {
            initialized_ticks[0].tick = 1;
        }
        assert!(StoragePlanValidator::validate(&unaligned).is_err());

        let mut out_of_range = plan.clone();
        if let StorageReadPlan::V3 { initialized_ticks, .. } = &mut out_of_range {
            initialized_ticks[0].tick = MAX_V3_TICK + 1;
        }
        assert!(StoragePlanValidator::validate(&out_of_range).is_err());
        let snapshot =
            universe(vec![descriptor(20, ExactProtocol::UniswapV3, plan.clone(), 3_000)]);
        let state = materialized(&snapshot, |_, slot| match slot.as_limbs()[0] {
            1 => U256::from(1) << 96,
            2 => U256::from(1_000),
            value if value == bitmap_slot => U256::from(1),
            value if value == liquidity_gross_slot => U256::from(100),
            value if value == liquidity_net_slot => (U256::from(1) << 128) - U256::from(1),
            _ => U256::ZERO,
        });
        let prepared =
            PoolStatePreparer::prepare(&snapshot, &state, &probe()).expect("prepared v3");
        assert_eq!(
            prepared[0].quote,
            PreparedPoolQuote::v3(
                U256::from(1) << 96,
                U256::from(1_000),
                0,
                tick_spacing,
                vec![PairwiseV3Tick { tick: 0, liquidity_net: I256::from_raw(U256::MAX) }],
            )
        );

        let mut missing = state.clone();
        missing.writes.retain(|write| write.key.slot() != Some(U256::from(bitmap_slot)));
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &missing, &probe()),
            Err(PreparationError::V3Coverage)
        );

        let mut wrong_bitmap = state.clone();
        wrong_bitmap
            .writes
            .iter_mut()
            .find(|write| write.key.slot() == Some(U256::from(bitmap_slot)))
            .expect("bitmap")
            .value = U256::ZERO;
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &wrong_bitmap, &probe()),
            Err(PreparationError::V3Coverage)
        );

        let mut nonzero_sentinel = state;
        nonzero_sentinel
            .writes
            .iter_mut()
            .find(|write| write.key.slot() == Some(U256::from(lower_sentinel_slot)))
            .expect("sentinel")
            .value = U256::from(1);
        assert_eq!(
            PoolStatePreparer::prepare(&snapshot, &nonzero_sentinel, &probe()),
            Err(PreparationError::V3Coverage)
        );

        let mut narrowed = plan;
        if let StorageReadPlan::V3 { lower_word, words, lower_sentinel, coverage_digest, .. } =
            &mut narrowed
        {
            *lower_word += 1;
            let omitted = words.remove(0);
            *lower_sentinel = omitted;
            *coverage_digest = B256::ZERO;
        }
        let narrowed_digest = CoverageHasher::digest(&narrowed).expect("narrow coverage digest");
        if let StorageReadPlan::V3 { coverage_digest, .. } = &mut narrowed {
            *coverage_digest = narrowed_digest;
        }
        let narrowed_snapshot =
            universe(vec![descriptor(21, ExactProtocol::UniswapV3, narrowed, 3_000)]);
        let narrowed_state = materialized(&narrowed_snapshot, |_, _| U256::ZERO);
        assert_eq!(
            PoolStatePreparer::prepare(&narrowed_snapshot, &narrowed_state, &probe()),
            Err(PreparationError::V3Coverage)
        );
    }
}
