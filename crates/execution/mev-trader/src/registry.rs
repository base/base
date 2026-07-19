use alloy_primitives::{Address, B256, U256};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{VisitControl, VisitSummary};

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
    /// Protocol fee in protocol-native fixed units.
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
}
