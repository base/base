//! Header type for Base chains.

use alloc::vec::Vec;
use core::mem;

use alloy_consensus::{BlockHeader as AlloyBlockHeader, Header, InMemorySize};
use alloy_primitives::{
    Address, B64, B256, BlockNumber, Bloom, Bytes, Sealable, Sealed, U256, keccak256,
};
use alloy_rlp::{BufMut, Decodable, Encodable};

/// Number of milliseconds in one Unix timestamp second.
pub const TIMESTAMP_MILLIS_PER_SECOND: u16 = 1_000;

/// Base block cadence in milliseconds for sub-second header timing.
pub const BASE_BLOCK_TIME_MILLIS: u16 = 200;

/// Valid millisecond subsecond components for 200ms Base headers.
pub const VALID_TIMESTAMP_MILLIS_PARTS: [u16; 5] = [0, 200, 400, 600, 800];

/// Error returned when a Base header millisecond timestamp component is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimestampMillisPartError {
    /// Validation for sub-second header timing requires a millisecond component.
    #[error("timestamp millisecond part is required when sub-second timing is active")]
    MissingPart,

    /// Millisecond component is not aligned to the 200ms cadence.
    #[error("timestamp millisecond part must be one of 0, 200, 400, 600, or 800, got {0}")]
    InvalidPart(u16),

    /// Header seconds timestamp cannot be represented as milliseconds.
    #[error("timestamp seconds overflowed when converted to milliseconds")]
    TimestampOverflow,

    /// Child timestamp seconds cannot be lower than parent timestamp seconds.
    #[error("child timestamp seconds {child} is lower than parent timestamp seconds {parent}")]
    ParentSecondsAfterChild {
        /// Child header seconds timestamp.
        child: u64,
        /// Parent header seconds timestamp.
        parent: u64,
    },

    /// Child millisecond timestamp must be greater than parent millisecond timestamp.
    #[error(
        "child timestamp milliseconds {child} must be greater than parent timestamp milliseconds {parent}"
    )]
    NonIncreasingTimestamp {
        /// Child header millisecond timestamp.
        child: u64,
        /// Parent header millisecond timestamp.
        parent: u64,
    },

    /// Child millisecond timestamp must advance by an integer number of 200ms slots.
    #[error("timestamp millisecond delta {0} is not aligned to the 200ms block cadence")]
    NonSlotAlignedDelta(u64),
}

/// Base-owned header fields committed by the Base header boundary.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    alloy_rlp::RlpEncodable,
    alloy_rlp::RlpDecodable,
)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "camelCase"))]
#[rlp(trailing)]
pub struct BaseHeaderFields {
    /// Base-owned millisecond subsecond component committed by the header hash.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub timestamp_millis_part: Option<u16>,
}

impl BaseHeaderFields {
    /// Creates Base-owned header fields.
    pub const fn new(timestamp_millis_part: Option<u16>) -> Self {
        Self { timestamp_millis_part }
    }

    /// Returns `true` when no Base-owned fields are populated.
    pub const fn is_empty(&self) -> bool {
        self.timestamp_millis_part.is_none()
    }

    /// Validates all populated Base-owned fields.
    pub fn validate(&self) -> Result<(), TimestampMillisPartError> {
        if let Some(part) = self.timestamp_millis_part {
            BaseHeader::validate_timestamp_millis_part(part)?;
        }

        Ok(())
    }
}

/// Base-owned RLP wrapper used for post-hardfork Base header encoding.
#[derive(Clone, Debug, PartialEq, Eq, Hash, alloy_rlp::RlpEncodable, alloy_rlp::RlpDecodable)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct BaseHeaderPayload {
    /// Standard Ethereum execution header fields.
    pub inner: Header,
    /// Base-owned post-hardfork header fields.
    pub base: BaseHeaderFields,
}

impl BaseHeaderPayload {
    /// Creates a post-fork payload from a Base header.
    pub fn from_header(header: &BaseHeader) -> Self {
        Self { inner: header.inner.clone(), base: header.base }
    }

    /// Converts the payload back into a Base header.
    pub fn try_into_header(self) -> Result<BaseHeader, TimestampMillisPartError> {
        BaseHeader::new(self.inner, self.base)
    }
}

/// Base header wrapper with Base-owned post-hardfork fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BaseHeader {
    /// Standard Ethereum execution header fields.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Header,
    /// Base-owned post-hardfork header fields.
    #[cfg_attr(feature = "serde", serde(flatten, default))]
    pub base: BaseHeaderFields,
}

impl BaseHeader {
    /// Creates a Base header and validates the Base-owned fields.
    pub fn new(inner: Header, base: BaseHeaderFields) -> Result<Self, TimestampMillisPartError> {
        base.validate()?;
        Ok(Self { inner, base })
    }

    /// Creates a Base header without validating its Base-owned fields.
    pub const fn new_unchecked(inner: Header, base: BaseHeaderFields) -> Self {
        Self { inner, base }
    }

    /// Returns true when the millisecond component is valid for a 200ms cadence.
    pub const fn is_valid_timestamp_millis_part(part: u16) -> bool {
        part < TIMESTAMP_MILLIS_PER_SECOND && part.is_multiple_of(BASE_BLOCK_TIME_MILLIS)
    }

    /// Validates a millisecond timestamp component.
    pub const fn validate_timestamp_millis_part(part: u16) -> Result<(), TimestampMillisPartError> {
        if Self::is_valid_timestamp_millis_part(part) {
            Ok(())
        } else {
            Err(TimestampMillisPartError::InvalidPart(part))
        }
    }

    /// Returns the canonical millisecond timestamp when the sub-second component is present.
    pub fn timestamp_millis(&self) -> Result<Option<u64>, TimestampMillisPartError> {
        let Some(part) = self.base.timestamp_millis_part else {
            return Ok(None);
        };

        Self::validate_timestamp_millis_part(part)?;

        let timestamp_seconds = self
            .inner
            .timestamp
            .checked_mul(u64::from(TIMESTAMP_MILLIS_PER_SECOND))
            .ok_or(TimestampMillisPartError::TimestampOverflow)?;

        Ok(Some(timestamp_seconds + u64::from(part)))
    }

    /// Returns the canonical millisecond timestamp when the sub-second component is required.
    pub fn required_timestamp_millis(&self) -> Result<u64, TimestampMillisPartError> {
        self.timestamp_millis()?.ok_or(TimestampMillisPartError::MissingPart)
    }

    /// Returns `true` when this header uses the legacy pre-hardfork encoding boundary.
    pub const fn is_legacy(&self) -> bool {
        self.base.is_empty()
    }

    /// Validates a child Base header timestamp against its parent when sub-second timing is active.
    pub fn validate_timestamp_millis_after(
        &self,
        parent: &Self,
    ) -> Result<(), TimestampMillisPartError> {
        if self.inner.timestamp < parent.inner.timestamp {
            return Err(TimestampMillisPartError::ParentSecondsAfterChild {
                child: self.inner.timestamp,
                parent: parent.inner.timestamp,
            });
        }

        let child_timestamp = self.required_timestamp_millis()?;
        let parent_timestamp = parent.required_timestamp_millis()?;

        if child_timestamp <= parent_timestamp {
            return Err(TimestampMillisPartError::NonIncreasingTimestamp {
                child: child_timestamp,
                parent: parent_timestamp,
            });
        }

        let delta = child_timestamp - parent_timestamp;
        if delta % u64::from(BASE_BLOCK_TIME_MILLIS) != 0 {
            return Err(TimestampMillisPartError::NonSlotAlignedDelta(delta));
        }

        Ok(())
    }

    /// Heavy function that calculates the Base header hash from its RLP encoding.
    pub fn hash_slow(&self) -> B256 {
        let mut out = Vec::<u8>::new();
        self.encode(&mut out);
        keccak256(&out)
    }

    /// Seal the Base header with a known hash.
    ///
    /// WARNING: This method does not validate whether the hash is correct.
    #[inline]
    pub const fn seal(self, hash: B256) -> Sealed<Self> {
        Sealed::new_unchecked(self, hash)
    }

    /// Consumes the wrapper and returns the nested Ethereum header.
    pub fn into_inner(self) -> Header {
        self.inner
    }
}

impl From<Header> for BaseHeader {
    fn from(inner: Header) -> Self {
        Self { inner, base: BaseHeaderFields::default() }
    }
}

impl AsRef<Header> for BaseHeader {
    fn as_ref(&self) -> &Header {
        &self.inner
    }
}

impl AsRef<Self> for BaseHeader {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl Sealable for BaseHeader {
    fn hash_slow(&self) -> B256 {
        Self::hash_slow(self)
    }
}

impl InMemorySize for BaseHeader {
    #[inline]
    fn size(&self) -> usize {
        self.inner.size() + mem::size_of::<BaseHeaderFields>()
    }
}

impl AlloyBlockHeader for BaseHeader {
    fn parent_hash(&self) -> B256 {
        self.inner.parent_hash()
    }

    fn ommers_hash(&self) -> B256 {
        self.inner.ommers_hash()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn state_root(&self) -> B256 {
        self.inner.state_root()
    }

    fn transactions_root(&self) -> B256 {
        self.inner.transactions_root()
    }

    fn receipts_root(&self) -> B256 {
        self.inner.receipts_root()
    }

    fn withdrawals_root(&self) -> Option<B256> {
        self.inner.withdrawals_root()
    }

    fn logs_bloom(&self) -> Bloom {
        self.inner.logs_bloom()
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn number(&self) -> BlockNumber {
        self.inner.number()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn mix_hash(&self) -> Option<B256> {
        self.inner.mix_hash()
    }

    fn nonce(&self) -> Option<B64> {
        self.inner.nonce()
    }

    fn base_fee_per_gas(&self) -> Option<u64> {
        self.inner.base_fee_per_gas()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn excess_blob_gas(&self) -> Option<u64> {
        self.inner.excess_blob_gas()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn requests_hash(&self) -> Option<B256> {
        self.inner.requests_hash()
    }

    fn block_access_list_hash(&self) -> Option<B256> {
        self.inner.block_access_list_hash()
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number()
    }

    fn extra_data(&self) -> &Bytes {
        self.inner.extra_data()
    }
}

impl Encodable for BaseHeader {
    fn encode(&self, out: &mut dyn BufMut) {
        self.base.validate().expect("invalid Base header fields");

        if self.is_legacy() {
            self.inner.encode(out);
        } else {
            BaseHeaderPayload::from_header(self).encode(out);
        }
    }

    fn length(&self) -> usize {
        self.base.validate().expect("invalid Base header fields");

        if self.is_legacy() {
            self.inner.length()
        } else {
            BaseHeaderPayload::from_header(self).length()
        }
    }
}

impl Decodable for BaseHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let mut probe = *buf;
        let outer = alloy_rlp::Header::decode(&mut probe)?;
        if !outer.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        let Some(first_payload_byte) = probe.first() else {
            return Err(alloy_rlp::Error::InputTooShort);
        };

        if *first_payload_byte < 0xC0 {
            return Header::decode(buf).map(Self::from);
        }

        let payload = BaseHeaderPayload::decode(buf)?;
        let header = payload
            .try_into_header()
            .map_err(|_| alloy_rlp::Error::Custom("invalid Base header payload fields"))?;

        if header.base.is_empty() {
            return Err(alloy_rlp::Error::Custom(
                "Base header payload must contain Base-owned fields",
            ));
        }

        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_rlp::{Decodable, Encodable};

    use super::*;

    #[test]
    fn timestamp_millis_part_validation_accepts_200ms_cadence() {
        for valid_part in VALID_TIMESTAMP_MILLIS_PARTS {
            assert!(BaseHeader::is_valid_timestamp_millis_part(valid_part));
            assert_eq!(BaseHeader::validate_timestamp_millis_part(valid_part), Ok(()));
        }
    }

    #[test]
    fn timestamp_millis_part_validation_rejects_out_of_cadence_parts() {
        for invalid_part in [1, 100, 199, 201, 999, 1_000] {
            assert!(!BaseHeader::is_valid_timestamp_millis_part(invalid_part));
            assert_eq!(
                BaseHeader::validate_timestamp_millis_part(invalid_part),
                Err(TimestampMillisPartError::InvalidPart(invalid_part))
            );
        }
    }

    #[test]
    fn timestamp_millis_combines_header_seconds_and_part() {
        let header = Header { timestamp: 1_234, ..Default::default() };
        let base_header = BaseHeader::new(header, BaseHeaderFields::new(Some(600))).unwrap();

        assert_eq!(base_header.timestamp_millis(), Ok(Some(1_234_600)));
    }

    #[test]
    fn timestamp_millis_is_absent_before_beryl() {
        let header = Header { timestamp: 1_234, ..Default::default() };
        let base_header = BaseHeader::new(header, BaseHeaderFields::default()).unwrap();

        assert_eq!(base_header.timestamp_millis(), Ok(None));
    }

    #[test]
    fn timestamp_millis_validation_accepts_same_second_sequence() {
        let parent = BaseHeader::new(
            Header { timestamp: 10, ..Default::default() },
            BaseHeaderFields::new(Some(0)),
        )
        .unwrap();
        let child = BaseHeader::new(
            Header { timestamp: 10, ..Default::default() },
            BaseHeaderFields::new(Some(200)),
        )
        .unwrap();

        assert_eq!(child.validate_timestamp_millis_after(&parent), Ok(()));
    }

    #[test]
    fn timestamp_millis_validation_accepts_second_rollover() {
        let parent = BaseHeader::new(
            Header { timestamp: 10, ..Default::default() },
            BaseHeaderFields::new(Some(800)),
        )
        .unwrap();
        let child = BaseHeader::new(
            Header { timestamp: 11, ..Default::default() },
            BaseHeaderFields::new(Some(0)),
        )
        .unwrap();

        assert_eq!(child.validate_timestamp_millis_after(&parent), Ok(()));
    }

    #[test]
    fn timestamp_millis_validation_rejects_duplicate_millis() {
        let parent = BaseHeader::new(
            Header { timestamp: 10, ..Default::default() },
            BaseHeaderFields::new(Some(200)),
        )
        .unwrap();
        let child = BaseHeader::new(
            Header { timestamp: 10, ..Default::default() },
            BaseHeaderFields::new(Some(200)),
        )
        .unwrap();

        assert_eq!(
            child.validate_timestamp_millis_after(&parent),
            Err(TimestampMillisPartError::NonIncreasingTimestamp { child: 10_200, parent: 10_200 })
        );
    }

    #[test]
    fn legacy_rlp_matches_upstream_header_bytes() {
        let inner = Header {
            timestamp: 1_780_334_562,
            number: 42_000,
            gas_limit: 30_000_000,
            gas_used: 1_234,
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(0x11)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
            requests_hash: Some(B256::repeat_byte(0x33)),
            ..Default::default()
        };
        let header = BaseHeader::from(inner.clone());

        let mut base_encoded = Vec::new();
        header.encode(&mut base_encoded);

        let mut upstream_encoded = Vec::new();
        inner.encode(&mut upstream_encoded);

        assert_eq!(base_encoded, upstream_encoded);
    }

    #[test]
    fn legacy_rlp_decodes_as_empty_base_fields() {
        let inner = Header {
            timestamp: 1_780_334_562,
            number: 42_000,
            gas_limit: 30_000_000,
            gas_used: 1_234,
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(0x11)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
            requests_hash: Some(B256::repeat_byte(0x33)),
            ..Default::default()
        };
        let mut encoded = Vec::new();
        inner.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let decoded = BaseHeader::decode(&mut slice).unwrap();

        assert!(slice.is_empty());
        assert_eq!(decoded.inner, inner);
        assert_eq!(decoded.base, BaseHeaderFields::default());
    }

    #[test]
    fn post_fork_rlp_round_trips_base_fields() {
        let header = BaseHeader::new(
            Header { timestamp: 1_234, ..Default::default() },
            BaseHeaderFields::new(Some(600)),
        )
        .unwrap();

        let mut encoded = Vec::new();
        header.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let decoded = BaseHeader::decode(&mut slice).unwrap();

        assert!(slice.is_empty());
        assert_eq!(decoded, header);
    }
}
