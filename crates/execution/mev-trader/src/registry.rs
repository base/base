use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, U256};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{MAX_ACCOUNTS, MAX_POOLS, MAX_STORAGE_SLOTS, VisitControl, VisitSummary};

/// One registry-audited state key that victim execution may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuditedWriteKey {
    /// An account balance write.
    AccountBalance {
        /// Account whose balance may change.
        address: Address,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
    /// An account nonce write.
    AccountNonce {
        /// Account whose nonce may change.
        address: Address,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
    /// A contract storage write.
    Storage {
        /// Contract whose storage may change.
        address: Address,
        /// Exact storage slot that may change.
        slot: U256,
        /// Digest of the audit evidence authorizing the key.
        evidence_digest: B256,
    },
}

impl AuditedWriteKey {
    /// Returns the account or contract address owned by this key.
    pub const fn address(&self) -> Address {
        match self {
            Self::AccountBalance { address, .. }
            | Self::AccountNonce { address, .. }
            | Self::Storage { address, .. } => *address,
        }
    }

    /// Returns the storage slot only for storage writes.
    pub const fn slot(&self) -> Option<U256> {
        match self {
            Self::Storage { slot, .. } => Some(*slot),
            Self::AccountBalance { .. } | Self::AccountNonce { .. } => None,
        }
    }

    /// Returns the audit evidence digest.
    pub const fn evidence_digest(&self) -> B256 {
        match self {
            Self::AccountBalance { evidence_digest, .. }
            | Self::AccountNonce { evidence_digest, .. }
            | Self::Storage { evidence_digest, .. } => *evidence_digest,
        }
    }
}

/// Immutable conflict-free write authority and quote-slot ownership for one pool universe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameAuditPlan {
    audited_writes: Vec<AuditedWriteKey>,
    quote_slot_owner: BTreeMap<(Address, U256), Address>,
    pools: Vec<Address>,
}

impl FrameAuditPlan {
    /// Constructs one canonical conflict-free audit plan.
    pub fn new(
        mut audited_writes: Vec<AuditedWriteKey>,
        quote_slot_owner: BTreeMap<(Address, U256), Address>,
    ) -> Result<Self, RegistryError> {
        audited_writes.sort_unstable();
        let mut logical = BTreeMap::<(u8, Address, U256), AuditedWriteKey>::new();
        for key in audited_writes {
            if key.evidence_digest().is_zero() {
                return Err(RegistryError::NonCanonical);
            }
            let identity = match key {
                AuditedWriteKey::AccountBalance { address, .. } => (0, address, U256::ZERO),
                AuditedWriteKey::AccountNonce { address, .. } => (1, address, U256::ZERO),
                AuditedWriteKey::Storage { address, slot, .. } => (2, address, slot),
            };
            if let Some(existing) = logical.insert(identity, key)
                && existing != key
            {
                return Err(RegistryError::AuditConflict);
            }
        }
        let account_count = logical.keys().filter(|(kind, _, _)| *kind != 2).count();
        let storage_count = logical.len().saturating_sub(account_count);
        if account_count > MAX_ACCOUNTS || storage_count > MAX_STORAGE_SLOTS {
            return Err(RegistryError::LimitExceeded);
        }
        for ((address, slot), owner) in &quote_slot_owner {
            if address != owner || !logical.contains_key(&(2, *address, *slot)) {
                return Err(RegistryError::MissingReadSlot);
            }
        }
        let mut pools = quote_slot_owner.values().copied().collect::<Vec<_>>();
        pools.sort_unstable();
        pools.dedup();
        Ok(Self { audited_writes: logical.into_values().collect(), quote_slot_owner, pools })
    }

    /// Returns the canonical audited-write union.
    pub fn audited_writes(&self) -> &[AuditedWriteKey] {
        &self.audited_writes
    }

    /// Returns the canonical pool-owned quote-slot map.
    pub const fn quote_slot_owners(&self) -> &BTreeMap<(Address, U256), Address> {
        &self.quote_slot_owner
    }

    /// Returns the independent canonical descriptor-pool universe.
    pub fn pools(&self) -> &[Address] {
        &self.pools
    }

    /// Returns the pool activated by an exact changed storage slot.
    pub fn quote_slot_owner(&self, address: Address, slot: U256) -> Option<Address> {
        self.quote_slot_owner.get(&(address, slot)).copied()
    }

    /// Returns the pool activated by an exact changed storage slot.
    pub fn owner_for_storage(&self, address: Address, slot: U256) -> Option<Address> {
        self.quote_slot_owner(address, slot)
    }

    /// Returns whether the audit plan contains quote-state authority for a pool.
    pub fn contains_pool(&self, pool: Address) -> bool {
        self.pools.binary_search(&pool).is_ok()
    }
}

/// Errors produced by deterministic registry validation and traversal.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// A required descriptor traversal stopped before exhaustion.
    #[error("required registry traversal stopped early")]
    VisitorStopped,
    /// A bounded registry representation exceeded its approved limit.
    #[error("registry limit exceeded")]
    LimitExceeded,
    /// Descriptor fields or arrays are not in canonical form.
    #[error("registry data is not canonical")]
    NonCanonical,
    /// A descriptor, coverage, or registry digest does not match its contents.
    #[error("registry digest mismatch")]
    DigestMismatch,
    /// The descriptor protocol or storage shape is unsupported.
    #[error("registry descriptor unsupported")]
    Unsupported,
    /// The enabled registry contains no descriptors.
    #[error("registry is empty")]
    Empty,
    /// Two entries authorize the same logical key with conflicting evidence.
    #[error("registry audit evidence conflicts")]
    AuditConflict,
    /// A descriptor read slot is absent from its audited-write authority.
    #[error("registry read slot is not audited")]
    MissingReadSlot,
}

/// Exactly supported pool protocols.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExactProtocol {
    /// Uniswap V2 constant-product pool.
    UniswapV2 = 0,
    /// Aerodrome volatile constant-product pool.
    AerodromeVolatile = 1,
    /// Aerodrome stable pool.
    AerodromeStable = 2,
    /// Uniswap V3 concentrated-liquidity pool.
    UniswapV3 = 3,
}

/// Storage field represented by a descriptor read.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldKind {
    /// Constant-product reserve zero.
    Reserve0 = 0,
    /// Constant-product reserve one.
    Reserve1 = 1,
    /// Stable-pool mode flag.
    StableFlag = 2,
    /// V3 square-root price in Q96 form.
    SqrtPriceX96 = 3,
    /// V3 active liquidity.
    Liquidity = 4,
    /// V3 current tick.
    CurrentTick = 5,
    /// Initialized-tick gross liquidity.
    LiquidityGross = 6,
    /// Initialized-tick signed net liquidity.
    LiquidityNet = 7,
}

/// Canonical bit-level storage read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldRead {
    /// Semantic field represented by this read.
    pub kind: FieldKind,
    /// Exact storage slot.
    pub slot: U256,
    /// Least-significant bit offset within the slot.
    pub bit_offset: u16,
    /// Width of the field in bits.
    pub bit_width: u16,
    /// Whether the field uses signed two's-complement interpretation.
    pub signed: bool,
}

/// Canonical V3 bitmap-word read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitmapWordRead {
    /// Compressed tick word position.
    pub word_position: i16,
    /// Exact bitmap storage slot.
    pub slot: U256,
}

/// Canonical initialized V3 tick read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InitializedTickRead {
    /// Initialized tick in the supported V3 domain.
    pub tick: i32,
    /// Gross-liquidity field read.
    pub liquidity_gross: FieldRead,
    /// Signed net-liquidity field read.
    pub liquidity_net: FieldRead,
}

/// Complete constructor input for one V3 storage read plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3ReadPlan {
    /// Square-root-price field.
    pub sqrt_price_x96: FieldRead,
    /// Active-liquidity field.
    pub liquidity: FieldRead,
    /// Current-tick field.
    pub current_tick: FieldRead,
    /// Positive attested tick spacing.
    pub tick_spacing: i32,
    /// Inclusive lower prepared bitmap word.
    pub lower_word: i16,
    /// Inclusive upper prepared bitmap word.
    pub upper_word: i16,
    /// Contiguous prepared bitmap reads.
    pub words: Vec<BitmapWordRead>,
    /// Checked `lower_word - 1` zero sentinel.
    pub lower_sentinel: BitmapWordRead,
    /// Checked `upper_word + 1` zero sentinel.
    pub upper_sentinel: BitmapWordRead,
    /// Canonically ordered initialized ticks.
    pub initialized_ticks: Vec<InitializedTickRead>,
    /// Digest covering words, sentinels, and initialized ticks.
    pub coverage_digest: B256,
}

/// Exact storage plan for one supported pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageReadPlan {
    /// Constant-product reserve storage.
    ConstantProduct {
        /// Reserve-zero field.
        reserve0: FieldRead,
        /// Reserve-one field.
        reserve1: FieldRead,
    },
    /// Aerodrome stable reserve storage.
    Stable {
        /// Reserve-zero field.
        reserve0: FieldRead,
        /// Reserve-one field.
        reserve1: FieldRead,
        /// Stable-mode field.
        stable: FieldRead,
    },
    /// V3 price, liquidity, bitmap, sentinel, and initialized-tick storage.
    V3 {
        /// Square-root-price field.
        sqrt_price_x96: FieldRead,
        /// Active-liquidity field.
        liquidity: FieldRead,
        /// Current-tick field.
        current_tick: FieldRead,
        /// Positive attested tick spacing.
        tick_spacing: i32,
        /// Inclusive lower prepared bitmap word.
        lower_word: i16,
        /// Inclusive upper prepared bitmap word.
        upper_word: i16,
        /// Contiguous prepared bitmap reads.
        words: Vec<BitmapWordRead>,
        /// Checked `lower_word - 1` zero sentinel.
        lower_sentinel: BitmapWordRead,
        /// Checked `upper_word + 1` zero sentinel.
        upper_sentinel: BitmapWordRead,
        /// Canonically ordered initialized ticks.
        initialized_ticks: Vec<InitializedTickRead>,
        /// Digest covering words, sentinels, and initialized ticks.
        coverage_digest: B256,
    },
}

impl StorageReadPlan {
    /// Constructs a constant-product read plan without exposing enum fields across crates.
    pub const fn constant_product(reserve0: FieldRead, reserve1: FieldRead) -> Self {
        Self::ConstantProduct { reserve0, reserve1 }
    }

    /// Constructs an Aerodrome stable read plan without exposing enum fields across crates.
    pub const fn stable(reserve0: FieldRead, reserve1: FieldRead, stable: FieldRead) -> Self {
        Self::Stable { reserve0, reserve1, stable }
    }

    /// Constructs a complete V3 read plan without exposing enum fields across crates.
    pub fn v3(plan: V3ReadPlan) -> Self {
        Self::V3 {
            sqrt_price_x96: plan.sqrt_price_x96,
            liquidity: plan.liquidity,
            current_tick: plan.current_tick,
            tick_spacing: plan.tick_spacing,
            lower_word: plan.lower_word,
            upper_word: plan.upper_word,
            words: plan.words,
            lower_sentinel: plan.lower_sentinel,
            upper_sentinel: plan.upper_sentinel,
            initialized_ticks: plan.initialized_ticks,
            coverage_digest: plan.coverage_digest,
        }
    }
    /// Returns every pool-owned storage slot needed to decode this plan.
    pub fn storage_slots(&self) -> Vec<U256> {
        let mut slots = Vec::new();
        match self {
            Self::ConstantProduct { reserve0, reserve1 } => {
                slots.push(reserve0.slot);
                slots.push(reserve1.slot);
            }
            Self::Stable { reserve0, reserve1, stable } => {
                slots.push(reserve0.slot);
                slots.push(reserve1.slot);
                slots.push(stable.slot);
            }
            Self::V3 {
                sqrt_price_x96,
                liquidity,
                current_tick,
                words,
                lower_sentinel,
                upper_sentinel,
                initialized_ticks,
                ..
            } => {
                slots.push(sqrt_price_x96.slot);
                slots.push(liquidity.slot);
                slots.push(current_tick.slot);
                slots.extend(words.iter().map(|word| word.slot));
                slots.push(lower_sentinel.slot);
                slots.push(upper_sentinel.slot);
                for tick in initialized_ticks {
                    slots.push(tick.liquidity_gross.slot);
                    slots.push(tick.liquidity_net.slot);
                }
            }
        }
        slots.sort_unstable();
        slots.dedup();
        slots
    }
}

/// Descriptor-owned canonical plan digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DescriptorPlanDigest(pub B256);

/// Registry-owned digest over strict descriptor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegistryDigest(pub B256);

/// Complete deterministic pool descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDescriptor {
    /// Direct pool contract address.
    pub pool: Address,
    /// Exact supported protocol.
    pub protocol: ExactProtocol,
    /// Canonical token-zero address.
    pub token0: Address,
    /// Canonical token-one address.
    pub token1: Address,
    /// Token-zero decimals.
    pub decimals0: u8,
    /// Token-one decimals.
    pub decimals1: u8,
    /// Protocol fee in millionths (pips).
    pub fee: u32,
    /// Attested runtime code hash.
    pub code_hash: B256,
    /// Exact pool storage read plan.
    pub read_plan: StorageReadPlan,
    /// Canonically ordered audited victim write keys.
    pub audited_writes: Vec<AuditedWriteKey>,
    /// Descriptor-owned digest, excluded from its own hash input.
    pub descriptor_digest: DescriptorPlanDigest,
}

impl PoolDescriptor {
    /// Validates canonical fields, coverage, audited writes, and the self-excluding digest.
    pub fn validate(&self) -> Result<(), RegistryError> {
        if self.pool.is_zero()
            || self.token0.is_zero()
            || self.token1.is_zero()
            || self.token0 >= self.token1
            || self.code_hash.is_zero()
            || self.fee > crate::FEE_DENOMINATOR
            || (self.protocol == ExactProtocol::UniswapV3 && self.fee == crate::FEE_DENOMINATOR)
        {
            return Err(RegistryError::NonCanonical);
        }
        let protocol_matches_plan = matches!(
            (&self.protocol, &self.read_plan),
            (
                ExactProtocol::UniswapV2 | ExactProtocol::AerodromeVolatile,
                StorageReadPlan::ConstantProduct { .. }
            ) | (ExactProtocol::AerodromeStable, StorageReadPlan::Stable { .. })
                | (ExactProtocol::UniswapV3, StorageReadPlan::V3 { .. })
        );
        if !protocol_matches_plan {
            return Err(RegistryError::Unsupported);
        }
        let mut audited = self.audited_writes.clone();
        audited.sort_unstable();
        audited.dedup();
        if audited != self.audited_writes
            || audited.iter().any(|key| key.evidence_digest().is_zero())
        {
            return Err(RegistryError::NonCanonical);
        }
        StoragePlanValidator::validate(&self.read_plan)?;
        if DescriptorHasher::digest(self)? != self.descriptor_digest {
            return Err(RegistryError::DigestMismatch);
        }
        Ok(())
    }
}

/// Object-safe borrowed descriptor visitor.
pub trait PoolDescriptorVisitor: std::fmt::Debug {
    /// Visits one registry descriptor.
    fn visit(&mut self, descriptor: &PoolDescriptor) -> Result<VisitControl, RegistryError>;
}

/// Object-safe deterministic pool registry.
pub trait PoolRegistry: std::fmt::Debug + Send + Sync {
    /// Visits descriptors in strict canonical pool order.
    fn visit_descriptors(
        &self,
        visitor: &mut dyn PoolDescriptorVisitor,
    ) -> Result<VisitSummary, RegistryError>;

    /// Returns the registry-owned canonical digest.
    fn registry_digest(&self) -> RegistryDigest;
}

/// Visitor that owns every descriptor yielded during one registry traversal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnapshotCollector {
    /// Descriptors copied from the registry in traversal order.
    pub descriptors: Vec<PoolDescriptor>,
}

impl PoolDescriptorVisitor for SnapshotCollector {
    fn visit(&mut self, descriptor: &PoolDescriptor) -> Result<VisitControl, RegistryError> {
        if self.descriptors.len() >= MAX_POOLS {
            return Err(RegistryError::LimitExceeded);
        }
        self.descriptors.push(descriptor.clone());
        Ok(VisitControl::Continue)
    }
}

/// Immutable validated pool universe used throughout one runtime lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolUniverseSnapshot {
    descriptors: Vec<PoolDescriptor>,
    registry_digest: RegistryDigest,
    audit: FrameAuditPlan,
}

impl PoolUniverseSnapshot {
    /// Captures and validates exactly one complete registry traversal.
    pub fn capture(registry: &dyn PoolRegistry) -> Result<Self, RegistryError> {
        let mut collector = SnapshotCollector::default();
        let summary = registry.visit_descriptors(&mut collector)?;
        let count =
            u32::try_from(collector.descriptors.len()).map_err(|_| RegistryError::LimitExceeded)?;
        if !summary.complete || summary.visited != count {
            return Err(RegistryError::VisitorStopped);
        }
        if collector.descriptors.is_empty() {
            return Err(RegistryError::Empty);
        }
        if collector.descriptors.len() > MAX_POOLS {
            return Err(RegistryError::LimitExceeded);
        }
        for descriptor in &collector.descriptors {
            descriptor.validate()?;
        }
        if collector.descriptors.windows(2).any(|pair| pair[0].pool >= pair[1].pool) {
            return Err(RegistryError::NonCanonical);
        }
        let registry_digest = registry.registry_digest();
        if RegistryHasher::digest(&collector.descriptors)? != registry_digest {
            return Err(RegistryError::DigestMismatch);
        }
        let audit = Self::build_audit(&collector.descriptors)?;
        Ok(Self { descriptors: collector.descriptors, registry_digest, audit })
    }

    /// Returns descriptors in strict canonical pool-address order.
    pub fn descriptors(&self) -> &[PoolDescriptor] {
        &self.descriptors
    }

    /// Returns the validated registry content digest.
    pub const fn registry_digest(&self) -> RegistryDigest {
        self.registry_digest
    }

    /// Returns the immutable frame audit plan.
    pub const fn audit(&self) -> &FrameAuditPlan {
        &self.audit
    }

    fn build_audit(descriptors: &[PoolDescriptor]) -> Result<FrameAuditPlan, RegistryError> {
        let mut logical = BTreeMap::<(u8, Address, U256), AuditedWriteKey>::new();
        for descriptor in descriptors {
            for key in &descriptor.audited_writes {
                let identity = match key {
                    AuditedWriteKey::AccountBalance { address, .. } => (0, *address, U256::ZERO),
                    AuditedWriteKey::AccountNonce { address, .. } => (1, *address, U256::ZERO),
                    AuditedWriteKey::Storage { address, slot, .. } => (2, *address, *slot),
                };
                if let Some(existing) = logical.insert(identity, *key)
                    && existing != *key
                {
                    return Err(RegistryError::AuditConflict);
                }
            }
        }

        let account_count = logical.keys().filter(|(kind, _, _)| *kind != 2).count();
        let storage_count = logical.len().saturating_sub(account_count);
        if account_count > MAX_ACCOUNTS || storage_count > MAX_STORAGE_SLOTS {
            return Err(RegistryError::LimitExceeded);
        }

        let mut quote_slot_owner = BTreeMap::new();
        for descriptor in descriptors {
            for slot in descriptor.read_plan.storage_slots() {
                let identity = (2, descriptor.pool, slot);
                let descriptor_owns_slot = descriptor.audited_writes.iter().any(|key| {
                    matches!(
                        key,
                        AuditedWriteKey::Storage { address, slot: audited_slot, .. }
                            if *address == descriptor.pool && *audited_slot == slot
                    )
                });
                if !descriptor_owns_slot || !logical.contains_key(&identity) {
                    return Err(RegistryError::MissingReadSlot);
                }
                if let Some(owner) =
                    quote_slot_owner.insert((descriptor.pool, slot), descriptor.pool)
                    && owner != descriptor.pool
                {
                    return Err(RegistryError::AuditConflict);
                }
            }
        }

        let mut audited_writes: Vec<_> = logical.into_values().collect();
        audited_writes.sort_unstable();
        let pools = descriptors.iter().map(|descriptor| descriptor.pool).collect();
        Ok(FrameAuditPlan { audited_writes, quote_slot_owner, pools })
    }
}

/// Provisioned owned registry validated before it can enter the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedPoolRegistry {
    descriptors: Vec<PoolDescriptor>,
    registry_digest: RegistryDigest,
}

impl ProvisionedPoolRegistry {
    /// Validates provisioned descriptors, ordering, digest, and complete audit authority.
    pub fn new(
        descriptors: Vec<PoolDescriptor>,
        registry_digest: RegistryDigest,
    ) -> Result<Self, RegistryError> {
        let registry = Self { descriptors, registry_digest };
        PoolUniverseSnapshot::capture(&registry)?;
        Ok(registry)
    }

    /// Returns the number of provisioned descriptors.
    pub const fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// Returns whether no descriptors are provisioned.
    pub const fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

impl PoolRegistry for ProvisionedPoolRegistry {
    fn visit_descriptors(
        &self,
        visitor: &mut dyn PoolDescriptorVisitor,
    ) -> Result<VisitSummary, RegistryError> {
        let mut visited = 0u32;
        for descriptor in &self.descriptors {
            visited = visited.checked_add(1).ok_or(RegistryError::LimitExceeded)?;
            if visitor.visit(descriptor)? == VisitControl::Stop {
                return Ok(VisitSummary { visited, complete: false });
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }

    fn registry_digest(&self) -> RegistryDigest {
        self.registry_digest
    }
}

/// Deterministic direct-contract fixture registry used by Phase A.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixturePoolRegistry {
    descriptors: Vec<PoolDescriptor>,
    registry_digest: RegistryDigest,
}

impl FixturePoolRegistry {
    /// Injects already-audited direct pool descriptors after complete digest validation.
    pub fn new(
        descriptors: Vec<PoolDescriptor>,
        registry_digest: RegistryDigest,
    ) -> Result<Self, RegistryError> {
        if descriptors.len() > crate::MAX_POOLS {
            return Err(RegistryError::LimitExceeded);
        }
        for descriptor in &descriptors {
            descriptor.validate()?;
        }
        if descriptors.windows(2).any(|pair| pair[0].pool >= pair[1].pool) {
            return Err(RegistryError::NonCanonical);
        }
        if RegistryHasher::digest(&descriptors)? != registry_digest {
            return Err(RegistryError::DigestMismatch);
        }
        Ok(Self { descriptors, registry_digest })
    }

    /// Returns the number of validated fixture descriptors.
    pub const fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// Returns whether the fixture registry contains no pools.
    pub const fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

impl PoolRegistry for FixturePoolRegistry {
    fn visit_descriptors(
        &self,
        visitor: &mut dyn PoolDescriptorVisitor,
    ) -> Result<VisitSummary, RegistryError> {
        let mut visited = 0u32;
        for descriptor in &self.descriptors {
            visited = visited.checked_add(1).ok_or(RegistryError::LimitExceeded)?;
            if visitor.visit(descriptor)? == VisitControl::Stop {
                return Err(RegistryError::VisitorStopped);
            }
        }
        Ok(VisitSummary { visited, complete: true })
    }

    fn registry_digest(&self) -> RegistryDigest {
        self.registry_digest
    }
}

/// Bounded fixed-width canonical byte encoder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    /// Creates an encoder containing a fixed ASCII domain and its u32 length.
    pub fn with_domain(domain: &[u8]) -> Result<Self, RegistryError> {
        let mut encoder = Self::default();
        encoder.push_len(domain.len())?;
        encoder.push_bytes(domain)?;
        Ok(encoder)
    }

    /// Appends one byte.
    pub fn push_u8(&mut self, value: u8) -> Result<(), RegistryError> {
        self.push_bytes(&[value])
    }

    /// Appends one big-endian u16.
    pub fn push_u16(&mut self, value: u16) -> Result<(), RegistryError> {
        self.push_bytes(&value.to_be_bytes())
    }

    /// Appends one big-endian i16.
    pub fn push_i16(&mut self, value: i16) -> Result<(), RegistryError> {
        self.push_bytes(&value.to_be_bytes())
    }

    /// Appends one big-endian u32.
    pub fn push_u32(&mut self, value: u32) -> Result<(), RegistryError> {
        self.push_bytes(&value.to_be_bytes())
    }

    /// Appends one big-endian i32.
    pub fn push_i32(&mut self, value: i32) -> Result<(), RegistryError> {
        self.push_bytes(&value.to_be_bytes())
    }

    /// Appends a u32 collection length.
    pub fn push_len(&mut self, value: usize) -> Result<(), RegistryError> {
        self.push_u32(u32::try_from(value).map_err(|_| RegistryError::LimitExceeded)?)
    }

    /// Appends an address.
    pub fn push_address(&mut self, value: Address) -> Result<(), RegistryError> {
        self.push_bytes(value.as_slice())
    }

    /// Appends a fixed 32-byte value.
    pub fn push_b256(&mut self, value: B256) -> Result<(), RegistryError> {
        self.push_bytes(value.as_slice())
    }

    /// Appends a fixed 256-bit big-endian integer.
    pub fn push_u256(&mut self, value: U256) -> Result<(), RegistryError> {
        self.push_bytes(&value.to_be_bytes::<32>())
    }

    /// Appends bytes while enforcing the canonical byte cap before extension.
    pub fn push_bytes(&mut self, value: &[u8]) -> Result<(), RegistryError> {
        let new_len =
            self.bytes.len().checked_add(value.len()).ok_or(RegistryError::LimitExceeded)?;
        if new_len > crate::MAX_CANONICAL_BYTES {
            return Err(RegistryError::LimitExceeded);
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    /// Returns the bounded canonical bytes.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Canonical descriptor digest implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct DescriptorHasher;

impl DescriptorHasher {
    /// Computes a descriptor digest while excluding descriptor and coverage self-digests.
    pub fn digest(descriptor: &PoolDescriptor) -> Result<DescriptorPlanDigest, RegistryError> {
        let mut encoder = CanonicalEncoder::with_domain(b"mev-trader-descriptor-plan-v1")?;
        encoder.push_address(descriptor.pool)?;
        encoder.push_u8(descriptor.protocol as u8)?;
        encoder.push_address(descriptor.token0)?;
        encoder.push_address(descriptor.token1)?;
        encoder.push_u8(descriptor.decimals0)?;
        encoder.push_u8(descriptor.decimals1)?;
        encoder.push_u32(descriptor.fee)?;
        encoder.push_b256(descriptor.code_hash)?;
        StoragePlanCodec::encode(&mut encoder, &descriptor.read_plan)?;
        encoder.push_len(descriptor.audited_writes.len())?;
        for key in &descriptor.audited_writes {
            AuditedWriteCodec::encode(&mut encoder, *key)?;
        }
        Ok(DescriptorPlanDigest(CanonicalDigest::sha256(&encoder.finish())))
    }
}

/// Canonical registry digest implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegistryHasher;

impl RegistryHasher {
    /// Computes a registry digest from strict descriptor order and no registry self-field.
    pub fn digest(descriptors: &[PoolDescriptor]) -> Result<RegistryDigest, RegistryError> {
        let mut encoder = CanonicalEncoder::with_domain(b"mev-trader-registry-v1")?;
        encoder.push_len(descriptors.len())?;
        for descriptor in descriptors {
            encoder.push_b256(descriptor.descriptor_digest.0)?;
        }
        Ok(RegistryDigest(CanonicalDigest::sha256(&encoder.finish())))
    }
}

/// Canonical V3 coverage digest implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoverageHasher;

impl CoverageHasher {
    /// Computes a coverage digest excluding the coverage self-field.
    pub fn digest(plan: &StorageReadPlan) -> Result<B256, RegistryError> {
        let StorageReadPlan::V3 {
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
        let mut encoder = CanonicalEncoder::with_domain(b"mev-trader-v3-coverage-v1")?;
        encoder.push_i16(*lower_word)?;
        encoder.push_i16(*upper_word)?;
        encoder.push_len(words.len())?;
        for word in words {
            StoragePlanCodec::encode_word(&mut encoder, *word)?;
        }
        StoragePlanCodec::encode_word(&mut encoder, *lower_sentinel)?;
        StoragePlanCodec::encode_word(&mut encoder, *upper_sentinel)?;
        encoder.push_len(initialized_ticks.len())?;
        for tick in initialized_ticks {
            StoragePlanCodec::encode_tick(&mut encoder, *tick)?;
        }
        Ok(CanonicalDigest::sha256(&encoder.finish()))
    }
}

/// SHA-256 conversion for canonical registry bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct CanonicalDigest;

impl CanonicalDigest {
    /// Hashes canonical bytes into a fixed 32-byte value.
    pub fn sha256(bytes: &[u8]) -> B256 {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        B256::from(digest)
    }
}

/// Structural validation for all supported storage plans.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoragePlanValidator;

impl StoragePlanValidator {
    /// Validates field shapes and protocol-specific canonical arrays.
    pub fn validate(plan: &StorageReadPlan) -> Result<(), RegistryError> {
        match plan {
            StorageReadPlan::ConstantProduct { reserve0, reserve1 } => {
                Self::validate_field(*reserve0, FieldKind::Reserve0, false)?;
                Self::validate_field(*reserve1, FieldKind::Reserve1, false)
            }
            StorageReadPlan::Stable { reserve0, reserve1, stable } => {
                Self::validate_field(*reserve0, FieldKind::Reserve0, false)?;
                Self::validate_field(*reserve1, FieldKind::Reserve1, false)?;
                Self::validate_field(*stable, FieldKind::StableFlag, false)
            }
            StorageReadPlan::V3 {
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
                coverage_digest,
            } => {
                Self::validate_field(*sqrt_price_x96, FieldKind::SqrtPriceX96, false)?;
                Self::validate_field(*liquidity, FieldKind::Liquidity, false)?;
                Self::validate_field(*current_tick, FieldKind::CurrentTick, true)?;
                crate::V3StorageValidator::validate_structure(
                    *tick_spacing,
                    *lower_word,
                    *upper_word,
                    words,
                    *lower_sentinel,
                    *upper_sentinel,
                    initialized_ticks,
                )?;
                if CoverageHasher::digest(plan)? != *coverage_digest {
                    return Err(RegistryError::DigestMismatch);
                }
                Ok(())
            }
        }
    }

    /// Validates an exact field kind, signedness, and checked bit range.
    pub fn validate_field(
        field: FieldRead,
        expected_kind: FieldKind,
        signed: bool,
    ) -> Result<(), RegistryError> {
        let end =
            field.bit_offset.checked_add(field.bit_width).ok_or(RegistryError::NonCanonical)?;
        if field.kind != expected_kind
            || field.signed != signed
            || field.bit_width == 0
            || end > 256
        {
            return Err(RegistryError::NonCanonical);
        }
        Ok(())
    }
}

/// Canonical storage-plan byte encoding.
#[derive(Debug, Default, Clone, Copy)]
pub struct StoragePlanCodec;

impl StoragePlanCodec {
    /// Encodes one storage plan without any self-digest fields.
    pub fn encode(
        encoder: &mut CanonicalEncoder,
        plan: &StorageReadPlan,
    ) -> Result<(), RegistryError> {
        match plan {
            StorageReadPlan::ConstantProduct { reserve0, reserve1 } => {
                encoder.push_u8(0)?;
                Self::encode_field(encoder, *reserve0)?;
                Self::encode_field(encoder, *reserve1)
            }
            StorageReadPlan::Stable { reserve0, reserve1, stable } => {
                encoder.push_u8(1)?;
                Self::encode_field(encoder, *reserve0)?;
                Self::encode_field(encoder, *reserve1)?;
                Self::encode_field(encoder, *stable)
            }
            StorageReadPlan::V3 {
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
            } => {
                encoder.push_u8(2)?;
                Self::encode_field(encoder, *sqrt_price_x96)?;
                Self::encode_field(encoder, *liquidity)?;
                Self::encode_field(encoder, *current_tick)?;
                encoder.push_i32(*tick_spacing)?;
                encoder.push_i16(*lower_word)?;
                encoder.push_i16(*upper_word)?;
                encoder.push_len(words.len())?;
                for word in words {
                    Self::encode_word(encoder, *word)?;
                }
                Self::encode_word(encoder, *lower_sentinel)?;
                Self::encode_word(encoder, *upper_sentinel)?;
                encoder.push_len(initialized_ticks.len())?;
                for tick in initialized_ticks {
                    Self::encode_tick(encoder, *tick)?;
                }
                Ok(())
            }
        }
    }

    /// Encodes one fixed-width field read.
    pub fn encode_field(
        encoder: &mut CanonicalEncoder,
        field: FieldRead,
    ) -> Result<(), RegistryError> {
        encoder.push_u8(field.kind as u8)?;
        encoder.push_u256(field.slot)?;
        encoder.push_u16(field.bit_offset)?;
        encoder.push_u16(field.bit_width)?;
        encoder.push_u8(u8::from(field.signed))
    }

    /// Encodes one fixed-width bitmap-word read.
    pub fn encode_word(
        encoder: &mut CanonicalEncoder,
        word: BitmapWordRead,
    ) -> Result<(), RegistryError> {
        encoder.push_i16(word.word_position)?;
        encoder.push_u256(word.slot)
    }

    /// Encodes one fixed-width initialized-tick read.
    pub fn encode_tick(
        encoder: &mut CanonicalEncoder,
        tick: InitializedTickRead,
    ) -> Result<(), RegistryError> {
        encoder.push_i32(tick.tick)?;
        Self::encode_field(encoder, tick.liquidity_gross)?;
        Self::encode_field(encoder, tick.liquidity_net)
    }
}

/// Canonical audited-write byte encoding.
#[derive(Debug, Default, Clone, Copy)]
pub struct AuditedWriteCodec;

impl AuditedWriteCodec {
    /// Encodes one owned key with explicit optional-slot shape and evidence digest.
    pub fn encode(
        encoder: &mut CanonicalEncoder,
        key: AuditedWriteKey,
    ) -> Result<(), RegistryError> {
        match key {
            AuditedWriteKey::AccountBalance { address, evidence_digest } => {
                encoder.push_u8(0)?;
                encoder.push_address(address)?;
                encoder.push_u8(0)?;
                encoder.push_b256(evidence_digest)
            }
            AuditedWriteKey::AccountNonce { address, evidence_digest } => {
                encoder.push_u8(1)?;
                encoder.push_address(address)?;
                encoder.push_u8(0)?;
                encoder.push_b256(evidence_digest)
            }
            AuditedWriteKey::Storage { address, slot, evidence_digest } => {
                encoder.push_u8(2)?;
                encoder.push_address(address)?;
                encoder.push_u8(1)?;
                encoder.push_u256(slot)?;
                encoder.push_b256(evidence_digest)
            }
        }
    }
}

/// Deterministic audit data used by crate-unit processing fixtures.
#[cfg(test)]
pub(crate) mod test_utils {
    use alloy_primitives::{Address, B256};

    use super::AuditedWriteKey;

    const NONCE_EVIDENCE_DIGEST: B256 = B256::new([0x5a; 32]);

    /// Returns the sole deterministic sender-nonce audit fixture.
    pub(crate) fn audited_sender_nonce(address: Address) -> [AuditedWriteKey; 1] {
        [AuditedWriteKey::AccountNonce { address, evidence_digest: NONCE_EVIDENCE_DIGEST }]
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn field(kind: FieldKind, slot: u64) -> FieldRead {
        FieldRead { kind, slot: U256::from(slot), bit_offset: 0, bit_width: 112, signed: false }
    }

    fn descriptor(pool_byte: u8) -> PoolDescriptor {
        let pool = Address::with_last_byte(pool_byte);
        let mut descriptor = PoolDescriptor {
            pool,
            protocol: ExactProtocol::UniswapV2,
            token0: Address::with_last_byte(1),
            token1: Address::with_last_byte(2),
            decimals0: 6,
            decimals1: 18,
            fee: 3_000,
            code_hash: B256::with_last_byte(3),
            read_plan: StorageReadPlan::ConstantProduct {
                reserve0: field(FieldKind::Reserve0, 0),
                reserve1: field(FieldKind::Reserve1, 1),
            },
            audited_writes: vec![
                AuditedWriteKey::Storage {
                    address: pool,
                    slot: U256::ZERO,
                    evidence_digest: B256::with_last_byte(4),
                },
                AuditedWriteKey::Storage {
                    address: pool,
                    slot: U256::from(1),
                    evidence_digest: B256::with_last_byte(5),
                },
            ],
            descriptor_digest: DescriptorPlanDigest(B256::ZERO),
        };
        descriptor.descriptor_digest =
            DescriptorHasher::digest(&descriptor).expect("descriptor digest");
        descriptor
    }

    #[test]
    fn descriptor_digest_excludes_only_its_self_field() {
        let descriptor = descriptor(10);
        let expected = descriptor.descriptor_digest;
        let mut self_mutation = descriptor.clone();
        self_mutation.descriptor_digest = DescriptorPlanDigest(B256::with_last_byte(99));
        assert_eq!(DescriptorHasher::digest(&self_mutation).expect("digest"), expected);

        assert_eq!(
            expected.0,
            "0x454b5db65b285a801e1317a017b349ca1989c668dbeb50588626f642bb6aefd1"
                .parse::<B256>()
                .expect("descriptor golden")
        );

        let mut content_mutation = descriptor;
        content_mutation.fee += 1;
        assert_ne!(DescriptorHasher::digest(&content_mutation).expect("digest"), expected);
    }

    #[test]
    fn registry_single_multi_reorder_and_mutation_goldens() {
        let first = descriptor(10);
        let second = descriptor(11);
        let single = RegistryHasher::digest(std::slice::from_ref(&first)).expect("single");
        let multi = RegistryHasher::digest(&[first.clone(), second.clone()]).expect("multi");
        assert_ne!(single, multi);
        assert_eq!(
            single.0,
            "0x826b40c8acdb24c54b645f00ebf831d23d52419bfbcd36cf5efc19ab4aec8bd6"
                .parse::<B256>()
                .expect("single registry golden")
        );
        assert_eq!(
            multi.0,
            "0xbf46febe1bbd37803787d396f972fbd8c9203169f9c2628cc0d3309e27508d52"
                .parse::<B256>()
                .expect("multi registry golden")
        );
        assert_eq!(
            FixturePoolRegistry::new(vec![first.clone()], single)
                .expect("single registry")
                .registry_digest(),
            single
        );
        assert_eq!(
            FixturePoolRegistry::new(vec![first.clone(), second.clone()], multi)
                .expect("multi registry")
                .len(),
            2
        );

        let reordered =
            RegistryHasher::digest(&[second.clone(), first.clone()]).expect("reordered digest");
        assert_ne!(reordered, multi);
        assert_eq!(
            FixturePoolRegistry::new(vec![second, first.clone()], reordered),
            Err(RegistryError::NonCanonical)
        );

        let mut mutated = first;
        mutated.decimals0 += 1;
        assert_eq!(
            FixturePoolRegistry::new(vec![mutated], single),
            Err(RegistryError::DigestMismatch)
        );
    }

    #[test]
    fn registry_digest_is_strictly_descriptor_digest_owned() {
        let descriptor = descriptor(10);
        let expected = RegistryHasher::digest(std::slice::from_ref(&descriptor)).expect("digest");
        let mut self_field_only = descriptor;
        self_field_only.descriptor_digest = DescriptorPlanDigest(B256::with_last_byte(88));
        assert_ne!(
            RegistryHasher::digest(std::slice::from_ref(&self_field_only)).expect("digest"),
            expected
        );
    }

    #[test]
    fn coverage_digest_excludes_self_field_and_has_fixed_golden() {
        let field = |kind, signed| FieldRead {
            kind,
            slot: U256::ZERO,
            bit_offset: 0,
            bit_width: 128,
            signed,
        };
        let mut plan = StorageReadPlan::V3 {
            sqrt_price_x96: field(FieldKind::SqrtPriceX96, false),
            liquidity: field(FieldKind::Liquidity, false),
            current_tick: field(FieldKind::CurrentTick, true),
            tick_spacing: 1,
            lower_word: 0,
            upper_word: 2,
            words: (0..=2)
                .map(|word_position| BitmapWordRead {
                    word_position,
                    slot: U256::from(word_position as u64),
                })
                .collect(),
            lower_sentinel: BitmapWordRead { word_position: -1, slot: U256::from(10) },
            upper_sentinel: BitmapWordRead { word_position: 3, slot: U256::from(11) },
            initialized_ticks: Vec::new(),
            coverage_digest: B256::ZERO,
        };
        let expected = CoverageHasher::digest(&plan).expect("coverage digest");
        assert_eq!(
            expected,
            "0xe42039d673aa137ea956b4ac05799bc7d2178b5d565c80d6030d9cf26baf4030"
                .parse::<B256>()
                .expect("coverage golden")
        );
        if let StorageReadPlan::V3 { coverage_digest, .. } = &mut plan {
            *coverage_digest = B256::with_last_byte(99);
        }
        assert_eq!(CoverageHasher::digest(&plan).expect("self-excluded digest"), expected);
    }
    #[test]
    fn t4a_registry_snapshot_rejects_incomplete_noncanonical_or_digest_mismatch() {
        #[derive(Debug)]
        struct IncompleteRegistry {
            descriptor: PoolDescriptor,
        }

        impl PoolRegistry for IncompleteRegistry {
            fn visit_descriptors(
                &self,
                visitor: &mut dyn PoolDescriptorVisitor,
            ) -> Result<VisitSummary, RegistryError> {
                let _ = visitor.visit(&self.descriptor)?;
                Ok(VisitSummary { visited: 1, complete: false })
            }

            fn registry_digest(&self) -> RegistryDigest {
                RegistryHasher::digest(std::slice::from_ref(&self.descriptor)).expect("digest")
            }
        }

        #[derive(Debug)]
        struct OverCapRegistry {
            descriptor: PoolDescriptor,
        }

        impl PoolRegistry for OverCapRegistry {
            fn visit_descriptors(
                &self,
                visitor: &mut dyn PoolDescriptorVisitor,
            ) -> Result<VisitSummary, RegistryError> {
                for _ in 0..=MAX_POOLS {
                    let _ = visitor.visit(&self.descriptor)?;
                }
                Ok(VisitSummary { visited: 0, complete: true })
            }

            fn registry_digest(&self) -> RegistryDigest {
                RegistryDigest(B256::ZERO)
            }
        }

        let first = descriptor(10);
        assert_eq!(
            PoolUniverseSnapshot::capture(&IncompleteRegistry { descriptor: first.clone() }),
            Err(RegistryError::VisitorStopped)
        );
        assert_eq!(
            PoolUniverseSnapshot::capture(&OverCapRegistry { descriptor: first.clone() }),
            Err(RegistryError::LimitExceeded)
        );

        let duplicate = vec![first.clone(), first.clone()];
        let duplicate_registry = FixturePoolRegistry {
            registry_digest: RegistryHasher::digest(&duplicate).expect("duplicate digest"),
            descriptors: duplicate,
        };
        assert_eq!(
            PoolUniverseSnapshot::capture(&duplicate_registry),
            Err(RegistryError::NonCanonical)
        );

        let mut invalid = first.clone();
        invalid.code_hash = B256::ZERO;
        let invalid_registry = FixturePoolRegistry {
            registry_digest: RegistryHasher::digest(std::slice::from_ref(&invalid))
                .expect("invalid digest"),
            descriptors: vec![invalid],
        };
        assert_eq!(
            PoolUniverseSnapshot::capture(&invalid_registry),
            Err(RegistryError::NonCanonical)
        );

        let second = descriptor(11);
        let reverse = vec![second.clone(), first.clone()];
        let reverse_registry = FixturePoolRegistry {
            registry_digest: RegistryHasher::digest(&reverse).expect("reverse digest"),
            descriptors: reverse,
        };
        assert_eq!(
            PoolUniverseSnapshot::capture(&reverse_registry),
            Err(RegistryError::NonCanonical)
        );

        let mismatched_registry = FixturePoolRegistry {
            descriptors: vec![first, second],
            registry_digest: RegistryDigest(B256::with_last_byte(99)),
        };
        assert_eq!(
            PoolUniverseSnapshot::capture(&mismatched_registry),
            Err(RegistryError::DigestMismatch)
        );
    }

    #[test]
    fn t4a_audited_union_dedups_identical_and_rejects_logical_conflicts() {
        let shared = AuditedWriteKey::AccountNonce {
            address: Address::with_last_byte(90),
            evidence_digest: B256::with_last_byte(91),
        };
        let mut first = descriptor(10);
        first.audited_writes.push(shared);
        first.audited_writes.sort_unstable();
        first.descriptor_digest = DescriptorHasher::digest(&first).expect("first digest");
        let mut second = descriptor(11);
        second.audited_writes.push(shared);
        second.audited_writes.sort_unstable();
        second.descriptor_digest = DescriptorHasher::digest(&second).expect("second digest");

        let descriptors = vec![first.clone(), second.clone()];
        let digest = RegistryHasher::digest(&descriptors).expect("registry digest");
        let registry =
            FixturePoolRegistry::new(descriptors, digest).expect("conflict-free registry");
        let snapshot = PoolUniverseSnapshot::capture(&registry).expect("snapshot");
        assert_eq!(
            snapshot.audit().audited_writes().iter().filter(|key| **key == shared).count(),
            1
        );
        assert!(snapshot.audit().audited_writes().windows(2).all(|pair| pair[0] < pair[1]));

        let mut omitted = first.clone();
        let omitted_pool = omitted.pool;
        omitted.audited_writes.retain(|key| {
            !matches!(
                key,
                AuditedWriteKey::Storage { address, slot, .. }
                    if *address == omitted_pool && *slot == U256::from(1)
            )
        });
        omitted.descriptor_digest =
            DescriptorHasher::digest(&omitted).expect("omitted descriptor digest");
        let mut cross_supplier = second.clone();
        cross_supplier.audited_writes.push(AuditedWriteKey::Storage {
            address: omitted_pool,
            slot: U256::from(1),
            evidence_digest: B256::with_last_byte(5),
        });
        cross_supplier.audited_writes.sort_unstable();
        cross_supplier.descriptor_digest =
            DescriptorHasher::digest(&cross_supplier).expect("cross-supplier digest");
        let cross_supplied = vec![omitted, cross_supplier];
        let digest =
            RegistryHasher::digest(&cross_supplied).expect("cross-supplied registry digest");
        let registry =
            FixturePoolRegistry::new(cross_supplied, digest).expect("descriptor-valid registry");
        assert_eq!(PoolUniverseSnapshot::capture(&registry), Err(RegistryError::MissingReadSlot));
        second.audited_writes.retain(|key| *key != shared);
        second.audited_writes.push(AuditedWriteKey::AccountNonce {
            address: shared.address(),
            evidence_digest: B256::with_last_byte(92),
        });
        second.audited_writes.sort_unstable();
        second.descriptor_digest = DescriptorHasher::digest(&second).expect("conflict digest");
        let conflicting = vec![first, second];
        let digest = RegistryHasher::digest(&conflicting).expect("registry digest");
        let registry =
            FixturePoolRegistry::new(conflicting, digest).expect("descriptor-valid registry");
        assert_eq!(PoolUniverseSnapshot::capture(&registry), Err(RegistryError::AuditConflict));
    }
}
