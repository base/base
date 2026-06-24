//! Header type for Base chains.

use alloc::vec::Vec;
use core::mem;

use alloy_consensus::{BlockHeader as AlloyBlockHeader, Header, InMemorySize};
use alloy_primitives::{
    Address, B64, B256, BlockNumber, Bloom, Bytes, Sealable, Sealed, U256, keccak256,
};
use alloy_rlp::{BufMut, Decodable, Encodable, RlpDecodable, RlpEncodable};

/// Number of milliseconds in one Unix timestamp second.
pub const TIMESTAMP_MILLIS_PER_SECOND: u16 = 1_000;

/// Base block cadence in milliseconds when the sub-second timestamp component is used.
pub const BASE_BLOCK_TIME_MILLIS: u16 = 200;

/// Valid millisecond subsecond components for 200ms Base headers.
pub const VALID_TIMESTAMP_MILLIS_PARTS: [u16; 5] = [0, 200, 400, 600, 800];

/// Error returned when a header millisecond timestamp component is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimestampMillisPartError {
    /// Validation requires a millisecond component.
    #[error("timestamp millisecond part is required")]
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

/// Base header wrapper with an optional millisecond timestamp component.
///
/// `BaseHeader` is the canonical Base block header: it wraps the upstream Ethereum
/// [`Header`] fields and adds an optional `timestamp_millis_part`
/// committed by the header hash. When `timestamp_millis_part` is `None`, the RLP
/// encoding, hash, and reth `Compact` bytes are byte-identical to the upstream
/// [`Header`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BaseHeader {
    /// Standard Ethereum execution header fields.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Header,
    /// Optional millisecond subsecond component committed by the header hash.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub timestamp_millis_part: Option<u16>,
}

impl BaseHeader {
    /// Creates a Base header and validates the optional millisecond timestamp component.
    pub fn new(
        inner: Header,
        timestamp_millis_part: Option<u16>,
    ) -> Result<Self, TimestampMillisPartError> {
        if let Some(part) = timestamp_millis_part
            && !Self::is_valid_timestamp_millis_part(part)
        {
            return Err(TimestampMillisPartError::InvalidPart(part));
        }

        Ok(Self { inner, timestamp_millis_part })
    }

    /// Creates a Base header without validating its millisecond timestamp component.
    pub const fn new_unchecked(inner: Header, timestamp_millis_part: Option<u16>) -> Self {
        Self { inner, timestamp_millis_part }
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

    /// Returns the canonical timestamp in milliseconds when the sub-second component is present.
    pub fn timestamp_millis(&self) -> Result<Option<u64>, TimestampMillisPartError> {
        let Some(part) = self.timestamp_millis_part else {
            return Ok(None);
        };

        let timestamp_seconds = self
            .inner
            .timestamp
            .checked_mul(u64::from(TIMESTAMP_MILLIS_PER_SECOND))
            .ok_or(TimestampMillisPartError::TimestampOverflow)?;

        Ok(Some(timestamp_seconds + u64::from(part)))
    }

    /// Returns the canonical timestamp in milliseconds.
    pub fn required_timestamp_millis(&self) -> Result<u64, TimestampMillisPartError> {
        self.timestamp_millis()?.ok_or(TimestampMillisPartError::MissingPart)
    }

    /// Validates a child Base header timestamp against its parent.
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

    /// Returns the post-activation RLP payload when a millisecond component is present.
    ///
    /// Pre-activation headers (`timestamp_millis_part == None`) have no payload because they
    /// encode byte-identically to the upstream [`Header`].
    fn to_payload(&self) -> Option<BaseHeaderPayload> {
        self.timestamp_millis_part.map(|timestamp_millis_part| BaseHeaderPayload {
            inner: self.inner.clone(),
            timestamp_millis_part,
        })
    }
}

impl From<Header> for BaseHeader {
    fn from(inner: Header) -> Self {
        Self { inner, timestamp_millis_part: None }
    }
}

impl From<BaseHeader> for Header {
    fn from(header: BaseHeader) -> Self {
        header.inner
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
        self.inner.size() + mem::size_of::<Option<u16>>()
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

/// RLP body of a post-activation Base header.
///
/// Wire layout: `[inner, timestamp_millis_part, <future trailing-optional fields>]`.
///
/// The upstream Ethereum [`Header`] is nested verbatim as the first element, so this format can
/// never drift from upstream's field layout. Nesting it first is also the decode discriminator:
/// a plain upstream header begins with `parent_hash` (an RLP string), while this begins with the
/// nested header (an RLP list). Base-specific fields follow the nested header; new ones are added
/// as trailing optionals (enabled by `#[rlp(trailing)]`), mirroring how Ethereum fork fields and
/// Tempo's consensus context grow a header without breaking previously encoded blocks.
#[derive(Clone, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
#[rlp(trailing)]
pub struct BaseHeaderPayload {
    /// Nested upstream Ethereum header, reused verbatim.
    pub inner: Header,
    /// Millisecond subsecond component of the block timestamp.
    pub timestamp_millis_part: u16,
}

impl Encodable for BaseHeader {
    fn encode(&self, out: &mut dyn BufMut) {
        let Some(payload) = self.to_payload() else {
            // Pre-activation headers are byte-identical to the upstream header, so historical
            // blocks keep their original encoding and hash.
            self.inner.encode(out);
            return;
        };
        payload.encode(out);
    }

    fn length(&self) -> usize {
        let Some(payload) = self.to_payload() else {
            return self.inner.length();
        };
        payload.length()
    }
}

impl Decodable for BaseHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Peek the outer list header and its first inner item without consuming `buf`.
        let mut probe = *buf;
        let outer = alloy_rlp::Header::decode(&mut probe)?;
        if !outer.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }

        // A plain upstream header starts with `parent_hash` (a 32-byte RLP string, first byte
        // `< 0xC0`); a post-activation Base header starts with the nested upstream header (an RLP
        // list, first byte `>= 0xC0`). This is the discriminator between the two encodings.
        let is_nested = probe.first().is_some_and(|first| *first >= 0xC0);
        if !is_nested {
            return Ok(Self::from(Header::decode(buf)?));
        }

        let payload = BaseHeaderPayload::decode(buf)?;
        Self::validate_timestamp_millis_part(payload.timestamp_millis_part)
            .map_err(|_| alloy_rlp::Error::Custom("invalid base header timestamp_millis_part"))?;

        Ok(Self {
            inner: payload.inner,
            timestamp_millis_part: Some(payload.timestamp_millis_part),
        })
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
        let base_header = BaseHeader::new(header, Some(600)).unwrap();

        assert_eq!(base_header.timestamp_millis(), Ok(Some(1_234_600)));
    }

    #[test]
    fn timestamp_millis_is_absent_when_part_is_missing() {
        let header = Header { timestamp: 1_234, ..Default::default() };
        let base_header = BaseHeader::new(header, None).unwrap();

        assert_eq!(base_header.timestamp_millis(), Ok(None));
    }

    #[test]
    fn timestamp_millis_validation_accepts_same_second_sequence() {
        let parent =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(0)).unwrap();
        let child =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(200)).unwrap();

        assert_eq!(child.validate_timestamp_millis_after(&parent), Ok(()));
    }

    #[test]
    fn timestamp_millis_validation_accepts_second_rollover() {
        let parent =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(800)).unwrap();
        let child =
            BaseHeader::new(Header { timestamp: 11, ..Default::default() }, Some(0)).unwrap();

        assert_eq!(child.validate_timestamp_millis_after(&parent), Ok(()));
    }

    #[test]
    fn timestamp_millis_validation_rejects_duplicate_millis() {
        let parent =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(200)).unwrap();
        let child =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(200)).unwrap();

        assert_eq!(
            child.validate_timestamp_millis_after(&parent),
            Err(TimestampMillisPartError::NonIncreasingTimestamp { child: 10_200, parent: 10_200 })
        );
    }

    #[test]
    fn timestamp_millis_validation_rejects_backward_millis() {
        let parent =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(400)).unwrap();
        let child =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(200)).unwrap();

        assert_eq!(
            child.validate_timestamp_millis_after(&parent),
            Err(TimestampMillisPartError::NonIncreasingTimestamp { child: 10_200, parent: 10_400 })
        );
    }

    #[test]
    fn timestamp_millis_validation_rejects_non_slot_aligned_delta() {
        let parent =
            BaseHeader::new_unchecked(Header { timestamp: 10, ..Default::default() }, Some(0));
        let child =
            BaseHeader::new_unchecked(Header { timestamp: 10, ..Default::default() }, Some(100));

        assert_eq!(
            child.validate_timestamp_millis_after(&parent),
            Err(TimestampMillisPartError::NonSlotAlignedDelta(100))
        );
    }

    #[test]
    fn timestamp_millis_validation_rejects_parent_seconds_after_child() {
        let parent =
            BaseHeader::new(Header { timestamp: 11, ..Default::default() }, Some(0)).unwrap();
        let child =
            BaseHeader::new(Header { timestamp: 10, ..Default::default() }, Some(800)).unwrap();

        assert_eq!(
            child.validate_timestamp_millis_after(&parent),
            Err(TimestampMillisPartError::ParentSecondsAfterChild { child: 10, parent: 11 })
        );
    }

    #[test]
    fn base_header_without_millis_part_matches_alloy_header_encoding_and_hash() {
        let header =
            Header { timestamp: 42, number: 7, gas_limit: 30_000_000, ..Default::default() };
        let base_header = BaseHeader::new(header.clone(), None).unwrap();
        let mut alloy_encoding = Vec::new();
        let mut base_encoding = Vec::new();

        header.encode(&mut alloy_encoding);
        base_header.encode(&mut base_encoding);

        assert_eq!(base_encoding, alloy_encoding);
        assert_eq!(base_header.hash_slow(), header.hash_slow());
    }

    #[test]
    fn base_header_hash_commits_millis_part() {
        let header =
            Header { timestamp: 42, number: 7, gas_limit: 30_000_000, ..Default::default() };
        let no_millis_header = BaseHeader::new(header.clone(), None).unwrap();
        let zero_millis_header = BaseHeader::new(header.clone(), Some(0)).unwrap();
        let two_hundred_millis_header = BaseHeader::new(header, Some(200)).unwrap();

        assert_ne!(zero_millis_header.hash_slow(), no_millis_header.hash_slow());
        assert_ne!(two_hundred_millis_header.hash_slow(), zero_millis_header.hash_slow());
    }

    fn sample_post_subsecond_header() -> Header {
        Header {
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
        }
    }

    #[test]
    fn rlp_round_trip_with_none_millis_part() {
        let header = sample_post_subsecond_header();
        let base_header = BaseHeader::new(header, None).unwrap();
        let mut encoded = Vec::new();
        base_header.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let decoded = BaseHeader::decode(&mut slice).unwrap();
        assert!(slice.is_empty(), "decoder must consume the entire RLP list");
        assert_eq!(decoded, base_header);
    }

    #[test]
    fn rlp_round_trip_with_some_millis_part() {
        for part in VALID_TIMESTAMP_MILLIS_PARTS {
            let header = sample_post_subsecond_header();
            let base_header = BaseHeader::new(header, Some(part)).unwrap();
            let mut encoded = Vec::new();
            base_header.encode(&mut encoded);

            let mut slice = encoded.as_slice();
            let decoded = BaseHeader::decode(&mut slice).unwrap();
            assert!(slice.is_empty(), "decoder must consume the entire RLP list for part={part}");
            assert_eq!(decoded, base_header);
        }
    }

    #[test]
    fn base_header_some_encoding_matches_payload_wrapper() {
        // The post-activation `BaseHeader` encoding is exactly the `BaseHeaderPayload` wrapper,
        // which is the single source of truth for the growable wire layout.
        let header = sample_post_subsecond_header();
        let base_header = BaseHeader::new(header.clone(), Some(400)).unwrap();
        let payload = BaseHeaderPayload { inner: header, timestamp_millis_part: 400 };

        assert_eq!(alloy_rlp::encode(&base_header), alloy_rlp::encode(&payload));

        let encoded = alloy_rlp::encode(&payload);
        let decoded = BaseHeaderPayload::decode(&mut encoded.as_slice()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn rlp_length_matches_encoded_bytes() {
        for millis_part in [None, Some(0), Some(200), Some(800)] {
            let header = sample_post_subsecond_header();
            let base_header = BaseHeader::new(header, millis_part).unwrap();
            let mut encoded = Vec::new();
            base_header.encode(&mut encoded);

            let mut slice = encoded.as_slice();
            let rlp_header = alloy_rlp::Header::decode(&mut slice).unwrap();

            assert!(rlp_header.list);
            assert_eq!(slice.len(), rlp_header.payload_length);
            assert_eq!(base_header.length(), encoded.len());
        }
    }

    #[test]
    fn rlp_nesting_discriminator_holds() {
        // The decoder distinguishes the two encodings by the first item inside the outer list:
        // a plain header begins with `parent_hash` (an RLP string), while a millis-bearing Base
        // header begins with the nested upstream header (an RLP list).
        let header = sample_post_subsecond_header();

        let mut none_encoded = Vec::new();
        BaseHeader::new(header.clone(), None).unwrap().encode(&mut none_encoded);
        let mut none_payload = none_encoded.as_slice();
        alloy_rlp::Header::decode(&mut none_payload).unwrap();
        assert!(none_payload.first().is_some_and(|first| *first < 0xC0));

        let mut some_encoded = Vec::new();
        BaseHeader::new(header, Some(200)).unwrap().encode(&mut some_encoded);
        let mut some_payload = some_encoded.as_slice();
        alloy_rlp::Header::decode(&mut some_payload).unwrap();
        assert!(some_payload.first().is_some_and(|first| *first >= 0xC0));
    }

    #[test]
    fn rlp_decode_of_plain_header_yields_none_millis_part() {
        let header = sample_post_subsecond_header();
        let mut encoded = Vec::new();
        header.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let decoded = BaseHeader::decode(&mut slice).unwrap();
        assert!(slice.is_empty());
        assert_eq!(decoded.inner, header);
        assert_eq!(decoded.timestamp_millis_part, None);
    }

    #[test]
    fn rlp_decode_rejects_invalid_millis_part() {
        let header = sample_post_subsecond_header();
        // Manually craft a header with a trailing u16 outside the 200ms cadence.
        let base_header = BaseHeader::new_unchecked(header, Some(123));
        let mut encoded = Vec::new();
        base_header.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let err = BaseHeader::decode(&mut slice).expect_err("invalid millis part must be rejected");
        assert!(matches!(err, alloy_rlp::Error::Custom(_)), "got {err:?}");
    }

    #[test]
    fn rlp_decode_rejects_nested_header_with_trailing_element() {
        // `[header, millis, extra]` must be rejected: the nested form carries exactly two items.
        let header = sample_post_subsecond_header();
        let mut inner_encoded = Vec::new();
        header.encode(&mut inner_encoded);

        let part = 200u16;
        let extra = 1u16;
        let payload_length = inner_encoded.len() + part.length() + extra.length();

        let mut encoded = Vec::new();
        alloy_rlp::Header { list: true, payload_length }.encode(&mut encoded);
        encoded.extend_from_slice(&inner_encoded);
        part.encode(&mut encoded);
        extra.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        let err = BaseHeader::decode(&mut slice).expect_err("trailing element must be rejected");
        assert!(matches!(err, alloy_rlp::Error::ListLengthMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn rlp_decode_rejects_nested_first_item_that_is_not_a_header() {
        // `[[..], millis]` where the first list is not a valid header must be rejected rather
        // than silently misread.
        let bogus_inner: [u16; 2] = [1, 2];
        let part = 200u16;
        let inner_payload_length = bogus_inner.iter().map(|item| item.length()).sum::<usize>();
        let inner_length = alloy_rlp::Header { list: true, payload_length: inner_payload_length }
            .length_with_payload();
        let payload_length = inner_length + part.length();

        let mut encoded = Vec::new();
        alloy_rlp::Header { list: true, payload_length }.encode(&mut encoded);
        alloy_rlp::Header { list: true, payload_length: inner_payload_length }.encode(&mut encoded);
        for item in bogus_inner {
            item.encode(&mut encoded);
        }
        part.encode(&mut encoded);

        let mut slice = encoded.as_slice();
        assert!(BaseHeader::decode(&mut slice).is_err(), "non-header first item must be rejected");
    }

    #[test]
    fn block_header_trait_forwards_to_inner() {
        let inner = sample_post_subsecond_header();
        let base_header = BaseHeader::new(inner.clone(), Some(400)).unwrap();
        assert_eq!(AlloyBlockHeader::number(&base_header), inner.number);
        assert_eq!(AlloyBlockHeader::timestamp(&base_header), inner.timestamp);
        assert_eq!(AlloyBlockHeader::gas_limit(&base_header), inner.gas_limit);
        assert_eq!(AlloyBlockHeader::base_fee_per_gas(&base_header), inner.base_fee_per_gas);
        assert_eq!(AlloyBlockHeader::requests_hash(&base_header), inner.requests_hash);
    }

    #[test]
    fn in_memory_size_includes_millis_part_slot() {
        let inner = sample_post_subsecond_header();
        let base_header = BaseHeader::new(inner.clone(), Some(200)).unwrap();
        assert_eq!(base_header.size(), inner.size() + mem::size_of::<Option<u16>>());
    }
}
