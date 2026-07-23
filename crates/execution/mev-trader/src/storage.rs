use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use alloy_primitives::U256;
use revm::{database_interface::Database, state::EvmState};

use crate::{
    AuditedWriteKey, BitmapWordRead, CancellationProbe, FieldKind, InitializedTickRead,
    MAX_ACCOUNTS, MAX_STORAGE_SLOTS, MAX_TOTAL_TICKS, MAX_V3_BITMAP_WORDS, PortError,
    RegistryError, StorageReadPlan,
};

/// Minimum and maximum supported Uniswap V3 ticks.
pub const MIN_V3_TICK: i32 = -887_272;
/// Maximum supported Uniswap V3 tick.
pub const MAX_V3_TICK: i32 = 887_272;

/// One immutable value materialized after victim execution commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializedWrite {
    /// Registry-audited key represented by this value.
    pub key: AuditedWriteKey,
    /// Post-victim value for the audited key.
    pub value: U256,
}

/// Provider-free immutable state passed to later analysis stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedState {
    /// Canonically ordered post-victim values.
    pub writes: Vec<MaterializedWrite>,
}

/// Validates that execution changed only registry-audited keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeltaGuard;

impl DeltaGuard {
    /// Returns whether every changed key is uniquely authorized and contract code is unchanged.
    pub fn permits(state: &EvmState, audited_writes: &[AuditedWriteKey]) -> bool {
        let unique: BTreeSet<_> = audited_writes.iter().copied().collect();
        if unique.len() != audited_writes.len()
            || audited_writes.iter().any(|key| key.evidence_digest().is_zero())
        {
            return false;
        }

        for (address, account) in state {
            let original = account.original_info();
            if account.info.code_hash != original.code_hash {
                return false;
            }
            if account.info.balance != original.balance
                && !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::AccountBalance { address: allowed, .. } if allowed == address)
                })
            {
                return false
            }
            if account.info.nonce != original.nonce
                && !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::AccountNonce { address: allowed, .. } if allowed == address)
                })
            {
                return false
            }
            for (slot, _) in account.changed_storage_slots() {
                if !unique.iter().any(|key| {
                    matches!(key, AuditedWriteKey::Storage { address: allowed, slot: allowed_slot, .. } if allowed == address && allowed_slot == slot)
                }) {
                    return false
                }
            }
        }
        true
    }
}

/// Materializes audited post-commit values into provider-free storage.
#[derive(Debug, Default, Clone, Copy)]
pub struct StateMaterializer;

impl StateMaterializer {
    /// Reads every audited key after the sole database commit and returns canonical values.
    pub fn materialize<DB: Database>(
        database: &mut DB,
        audited_writes: &[AuditedWriteKey],
        cancellation: &CancellationProbe,
    ) -> Result<MaterializedState, PortError> {
        let account_count = audited_writes
            .iter()
            .filter(|key| !matches!(key, AuditedWriteKey::Storage { .. }))
            .count();
        let storage_count = audited_writes.len().saturating_sub(account_count);
        if account_count > MAX_ACCOUNTS || storage_count > MAX_STORAGE_SLOTS {
            return Err(PortError::LimitExceeded);
        }
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return Err(PortError::Incoherent);
        }

        let mut keys = audited_writes.to_vec();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() != audited_writes.len() {
            return Err(PortError::Incoherent);
        }

        let mut writes = Vec::with_capacity(keys.len());
        for key in keys {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(PortError::Incoherent);
            }
            let value = match key {
                AuditedWriteKey::AccountBalance { address, .. } => database
                    .basic(address)
                    .map_err(|_| PortError::ProviderUnavailable)?
                    .map_or(U256::ZERO, |account| account.balance),
                AuditedWriteKey::AccountNonce { address, .. } => database
                    .basic(address)
                    .map_err(|_| PortError::ProviderUnavailable)?
                    .map_or(U256::ZERO, |account| U256::from(account.nonce)),
                AuditedWriteKey::Storage { address, slot, .. } => {
                    database.storage(address, slot).map_err(|_| PortError::ProviderUnavailable)?
                }
            };
            writes.push(MaterializedWrite { key, value });
        }
        Ok(MaterializedState { writes })
    }
}

/// Immutable prepared V3 bitmap and initialized-tick state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3PreparedState {
    /// Canonically ordered prepared bitmap values.
    pub words: Vec<(i16, U256)>,
    /// Value read from the checked lower sentinel word.
    pub lower_sentinel: U256,
    /// Value read from the checked upper sentinel word.
    pub upper_sentinel: U256,
    /// Canonically ordered initialized-tick records.
    pub initialized_ticks: Vec<InitializedTickRead>,
}

/// Checked V3 compression, coverage, sentinel, and quote validation.
#[derive(Debug, Default, Clone, Copy)]
pub struct V3StorageValidator;

impl V3StorageValidator {
    /// Validates checked word distance, sentinels, tick domain, alignment, and canonical order.
    pub fn validate_structure(
        tick_spacing: i32,
        lower_word: i16,
        upper_word: i16,
        words: &[BitmapWordRead],
        lower_sentinel: BitmapWordRead,
        upper_sentinel: BitmapWordRead,
        initialized_ticks: &[InitializedTickRead],
    ) -> Result<(), RegistryError> {
        if tick_spacing <= 0 {
            return Err(RegistryError::NonCanonical);
        }
        let distance = upper_word.checked_sub(lower_word).ok_or(RegistryError::NonCanonical)?;
        if distance < 2 {
            return Err(RegistryError::NonCanonical);
        }
        let expected_lower = lower_word.checked_sub(1).ok_or(RegistryError::NonCanonical)?;
        let expected_upper = upper_word.checked_add(1).ok_or(RegistryError::NonCanonical)?;
        if lower_sentinel.word_position != expected_lower
            || upper_sentinel.word_position != expected_upper
        {
            return Err(RegistryError::NonCanonical);
        }
        let expected_words =
            usize::try_from(i32::from(distance) + 1).map_err(|_| RegistryError::LimitExceeded)?;
        if words.len() != expected_words || words.len() > MAX_V3_BITMAP_WORDS {
            return Err(RegistryError::LimitExceeded);
        }
        for (offset, word) in words.iter().enumerate() {
            let offset = i16::try_from(offset).map_err(|_| RegistryError::LimitExceeded)?;
            if word.word_position
                != lower_word.checked_add(offset).ok_or(RegistryError::NonCanonical)?
            {
                return Err(RegistryError::NonCanonical);
            }
        }
        if initialized_ticks.len() > MAX_TOTAL_TICKS
            || initialized_ticks.windows(2).any(|pair| pair[0].tick >= pair[1].tick)
        {
            return Err(RegistryError::NonCanonical);
        }
        let mut bitmap_slots = BTreeSet::new();
        for word in words.iter().copied().chain([lower_sentinel, upper_sentinel]) {
            if !bitmap_slots.insert(word.slot) {
                return Err(RegistryError::NonCanonical);
            }
        }
        for tick in initialized_ticks {
            if !(MIN_V3_TICK..=MAX_V3_TICK).contains(&tick.tick)
                || tick.tick.rem_euclid(tick_spacing) != 0
                || tick.liquidity_gross.kind != FieldKind::LiquidityGross
                || tick.liquidity_gross.signed
                || tick.liquidity_net.kind != FieldKind::LiquidityNet
                || !tick.liquidity_net.signed
            {
                return Err(RegistryError::NonCanonical);
            }
            crate::StoragePlanValidator::validate_field(
                tick.liquidity_gross,
                FieldKind::LiquidityGross,
                false,
            )?;
            crate::StoragePlanValidator::validate_field(
                tick.liquidity_net,
                FieldKind::LiquidityNet,
                true,
            )?;
            let (word, _) = Self::compressed_position(tick.tick, tick_spacing)?;
            if word < i32::from(lower_word) || word > i32::from(upper_word) {
                return Err(RegistryError::NonCanonical);
            }
        }
        Ok(())
    }

    /// Validates exact set-bit to initialized-tick correspondence and zero sentinels.
    pub fn validate_prepared(
        plan: &StorageReadPlan,
        prepared: &V3PreparedState,
        cancellation: &CancellationProbe,
    ) -> Result<(), RegistryError> {
        let StorageReadPlan::V3 {
            tick_spacing,
            lower_word,
            upper_word,
            words,
            lower_sentinel,
            upper_sentinel,
            initialized_ticks,
            ..
        } = plan
        else {
            return Err(RegistryError::Unsupported);
        };
        Self::validate_structure(
            *tick_spacing,
            *lower_word,
            *upper_word,
            words,
            *lower_sentinel,
            *upper_sentinel,
            initialized_ticks,
        )?;
        if !prepared.lower_sentinel.is_zero()
            || !prepared.upper_sentinel.is_zero()
            || prepared.initialized_ticks != *initialized_ticks
            || prepared.words.len() != words.len()
        {
            return Err(RegistryError::NonCanonical);
        }

        let mut expected = BTreeMap::<i16, U256>::new();
        for tick in initialized_ticks {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(RegistryError::VisitorStopped);
            }
            let (word, bit) = Self::compressed_position(tick.tick, *tick_spacing)?;
            let word = i16::try_from(word).map_err(|_| RegistryError::NonCanonical)?;
            let value = expected.entry(word).or_default();
            *value |= U256::from(1) << bit;
        }
        for ((prepared_position, prepared_value), descriptor_word) in
            prepared.words.iter().zip(words)
        {
            if !cancellation.checkpoint(Instant::now(), true) {
                cancellation.acknowledge_drop();
                return Err(RegistryError::VisitorStopped);
            }
            if prepared_position != &descriptor_word.word_position
                || *prepared_value != expected.get(prepared_position).copied().unwrap_or(U256::ZERO)
            {
                return Err(RegistryError::NonCanonical);
            }
        }
        Ok(())
    }

    /// Returns the Euclidean-floor compressed word and bit for one aligned tick.
    pub fn compressed_position(tick: i32, tick_spacing: i32) -> Result<(i32, u32), RegistryError> {
        if tick_spacing <= 0
            || !(MIN_V3_TICK..=MAX_V3_TICK).contains(&tick)
            || tick.rem_euclid(tick_spacing) != 0
        {
            return Err(RegistryError::NonCanonical);
        }
        let compressed = tick.div_euclid(tick_spacing);
        let word = compressed.div_euclid(256);
        let bit =
            u32::try_from(compressed.rem_euclid(256)).map_err(|_| RegistryError::NonCanonical)?;
        Ok((word, bit))
    }

    /// Allows quotes only when both aligned ticks stay in one strict interior word.
    pub fn allows_quote(
        plan: &StorageReadPlan,
        start_tick: i32,
        end_tick: i32,
        cancellation: &CancellationProbe,
    ) -> bool {
        let StorageReadPlan::V3 { tick_spacing, lower_word, upper_word, .. } = plan else {
            return false;
        };
        if !cancellation.checkpoint(Instant::now(), true) {
            cancellation.acknowledge_drop();
            return false;
        }
        let Ok((start_word, _)) = Self::compressed_position(start_tick, *tick_spacing) else {
            return false;
        };
        let Ok((end_word, _)) = Self::compressed_position(end_tick, *tick_spacing) else {
            return false;
        };
        start_word == end_word
            && start_word > i32::from(*lower_word)
            && start_word < i32::from(*upper_word)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{Address, B256};
    use revm::{database_interface::DatabaseCommit, state::Account};
    use revm_database::InMemoryDB;

    use super::*;
    use crate::{CancellationToken, FieldRead, GlobalLifecycle, StorageReadPlan, TaskState};

    fn evidence() -> B256 {
        B256::with_last_byte(1)
    }

    fn probe() -> CancellationProbe {
        CancellationProbe::new(
            Arc::new(CancellationToken::with_approved_deadline(Instant::now())),
            Arc::new(GlobalLifecycle::default()),
        )
    }

    fn field(kind: FieldKind, signed: bool) -> FieldRead {
        FieldRead { kind, slot: U256::ZERO, bit_offset: 0, bit_width: 128, signed }
    }

    fn v3_plan(
        lower_word: i16,
        upper_word: i16,
        ticks: Vec<InitializedTickRead>,
    ) -> StorageReadPlan {
        let words = (lower_word..=upper_word)
            .map(|word_position| BitmapWordRead {
                word_position,
                slot: U256::from(word_position as i32 as u32),
            })
            .collect();
        let mut plan = StorageReadPlan::V3 {
            sqrt_price_x96: field(FieldKind::SqrtPriceX96, false),
            liquidity: field(FieldKind::Liquidity, false),
            current_tick: field(FieldKind::CurrentTick, true),
            tick_spacing: 1,
            lower_word,
            upper_word,
            words,
            lower_sentinel: BitmapWordRead {
                word_position: lower_word.checked_sub(1).unwrap_or(lower_word),
                slot: U256::from(10),
            },
            upper_sentinel: BitmapWordRead {
                word_position: upper_word.checked_add(1).unwrap_or(upper_word),
                slot: U256::from(11),
            },
            initialized_ticks: ticks,
            coverage_digest: B256::ZERO,
        };
        let digest = crate::CoverageHasher::digest(&plan).expect("coverage digest");
        if let StorageReadPlan::V3 { coverage_digest, .. } = &mut plan {
            *coverage_digest = digest;
        }
        plan
    }

    fn initialized(tick: i32) -> InitializedTickRead {
        InitializedTickRead {
            tick,
            liquidity_gross: field(FieldKind::LiquidityGross, false),
            liquidity_net: field(FieldKind::LiquidityNet, true),
        }
    }

    #[test]
    fn delta_guard_rejects_write_outside_audited_subset() {
        let address = Address::with_last_byte(1);
        let mut account = Account::default();
        account.set_current_info_as_original();
        account.info.balance = U256::from(2);
        account.mark_touch();
        let state: EvmState = [(address, account)].into_iter().collect();

        assert!(!DeltaGuard::permits(&state, &[]));
        assert!(DeltaGuard::permits(
            &state,
            &[AuditedWriteKey::AccountBalance { address, evidence_digest: evidence() }]
        ));
    }

    #[test]
    fn delta_guard_rejects_code_change_even_when_account_write_is_audited() {
        let address = Address::with_last_byte(1);
        let mut account = Account::default();
        account.set_current_info_as_original();
        account.info.balance = U256::from(2);
        account.info.code_hash = B256::with_last_byte(2);
        account.mark_touch();
        let state: EvmState = [(address, account)].into_iter().collect();

        assert!(!DeltaGuard::permits(
            &state,
            &[AuditedWriteKey::AccountBalance { address, evidence_digest: evidence() }]
        ));
    }

    #[test]
    fn materializer_reads_only_after_commit() {
        let address = Address::with_last_byte(2);
        let key = AuditedWriteKey::AccountBalance { address, evidence_digest: evidence() };
        let mut account = Account::default();
        account.info.balance = U256::from(7);
        account.mark_touch();
        let mut database = InMemoryDB::default();

        database.commit([(address, account)].into_iter().collect());
        let materialized =
            StateMaterializer::materialize(&mut database, &[key], &probe()).expect("materialize");

        assert_eq!(materialized.writes, vec![MaterializedWrite { key, value: U256::from(7) }]);
    }

    #[test]
    fn materializer_drops_everything_after_cancellation() {
        let global = Arc::new(GlobalLifecycle::default());
        let token = Arc::new(CancellationToken::with_approved_deadline(Instant::now()));
        let cancellation = CancellationProbe::new(Arc::clone(&token), global);
        token.request_cancel();
        let result = StateMaterializer::materialize(&mut InMemoryDB::default(), &[], &cancellation);
        assert_eq!(result, Err(PortError::Incoherent));
        assert_eq!(token.state(), TaskState::DroppedAcked);
    }

    #[test]
    fn materialized_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MaterializedState>();
    }

    #[test]
    fn v3_rejects_equal_adjacent_reversed_and_extreme_bounds() {
        for (lower, upper) in
            [(0, 0), (0, 1), (1, 0), (i16::MIN, i16::MIN + 2), (i16::MAX - 2, i16::MAX)]
        {
            let plan = v3_plan(lower, upper, Vec::new());
            assert!(crate::StoragePlanValidator::validate(&plan).is_err());
        }
    }

    #[test]
    fn v3_validates_exact_bits_sentinels_domain_and_interior() {
        let ticks = vec![initialized(256), initialized(257)];
        let plan = v3_plan(0, 2, ticks.clone());
        crate::StoragePlanValidator::validate(&plan).expect("valid structure");
        let prepared = V3PreparedState {
            words: vec![(0, U256::ZERO), (1, U256::from(3)), (2, U256::ZERO)],
            lower_sentinel: U256::ZERO,
            upper_sentinel: U256::ZERO,
            initialized_ticks: ticks,
        };
        V3StorageValidator::validate_prepared(&plan, &prepared, &probe())
            .expect("valid prepared state");
        assert!(V3StorageValidator::allows_quote(&plan, 256, 257, &probe()));
        assert!(!V3StorageValidator::allows_quote(&plan, 0, 1, &probe()));
        assert!(!V3StorageValidator::allows_quote(&plan, 255, 256, &probe()));
        assert!(!V3StorageValidator::allows_quote(&plan, 512, 513, &probe()));

        let mut nonzero_sentinel = prepared.clone();
        nonzero_sentinel.lower_sentinel = U256::from(1);
        assert!(V3StorageValidator::validate_prepared(&plan, &nonzero_sentinel, &probe()).is_err());

        let mut wrong_bits = prepared;
        wrong_bits.words[1].1 = U256::from(1);
        assert!(V3StorageValidator::validate_prepared(&plan, &wrong_bits, &probe()).is_err());
    }

    #[test]
    fn v3_rejects_out_of_domain_misaligned_and_duplicate_bitmap_reads() {
        let mut out_of_domain = v3_plan(0, 2, vec![initialized(MAX_V3_TICK + 1)]);
        let digest = crate::CoverageHasher::digest(&out_of_domain).expect("coverage");
        if let StorageReadPlan::V3 { coverage_digest, .. } = &mut out_of_domain {
            *coverage_digest = digest;
        }
        assert!(crate::StoragePlanValidator::validate(&out_of_domain).is_err());

        let mut misaligned = v3_plan(0, 2, vec![initialized(257)]);
        if let StorageReadPlan::V3 { tick_spacing, .. } = &mut misaligned {
            *tick_spacing = 2;
        }
        assert!(crate::StoragePlanValidator::validate(&misaligned).is_err());

        let mut duplicate_slot = v3_plan(0, 2, Vec::new());
        if let StorageReadPlan::V3 { words, .. } = &mut duplicate_slot {
            words[1].slot = words[0].slot;
        }
        let digest = crate::CoverageHasher::digest(&duplicate_slot).expect("coverage");
        if let StorageReadPlan::V3 { coverage_digest, .. } = &mut duplicate_slot {
            *coverage_digest = digest;
        }
        assert!(crate::StoragePlanValidator::validate(&duplicate_slot).is_err());
    }

    #[test]
    fn negative_tick_compression_uses_euclidean_floor() {
        assert_eq!(V3StorageValidator::compressed_position(-1, 1), Ok((-1, 255)));
        assert_eq!(V3StorageValidator::compressed_position(-256, 1), Ok((-1, 0)));
        assert_eq!(V3StorageValidator::compressed_position(-257, 1), Ok((-2, 255)));
    }
}
