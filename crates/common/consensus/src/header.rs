//! Header type for Base chains.

use alloc::vec::Vec;

use alloy_consensus::Header;
use alloy_primitives::{B256, Sealable, Sealed, U256, keccak256};
use alloy_rlp::{BufMut, Encodable, length_of_length};

/// Number of milliseconds in one Unix timestamp second.
pub const TIMESTAMP_MILLIS_PER_SECOND: u16 = 1_000;

/// Base block cadence in milliseconds after Beryl.
pub const BASE_BLOCK_TIME_MILLIS: u16 = 200;

/// Valid millisecond subsecond components for 200ms Base headers.
pub const VALID_TIMESTAMP_MILLIS_PARTS: [u16; 5] = [0, 200, 400, 600, 800];

/// Error returned when a header millisecond timestamp component is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimestampMillisPartError {
    /// Post-Subsecond validation requires a millisecond component.
    #[error("timestamp millisecond part is required after Subsecond")]
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

/// Base header wrapper with an optional post-Beryl millisecond timestamp component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BaseHeader {
    /// Standard Ethereum execution header fields.
    pub inner: Header,
    /// Post-Beryl millisecond subsecond component committed by the header hash.
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

    /// Returns the canonical post-Beryl timestamp in milliseconds when present.
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

    /// Returns the canonical post-Beryl timestamp in milliseconds.
    pub fn required_timestamp_millis(&self) -> Result<u64, TimestampMillisPartError> {
        self.timestamp_millis()?.ok_or(TimestampMillisPartError::MissingPart)
    }

    /// Validates a child Base header timestamp against its parent for post-Beryl blocks.
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

    fn header_payload_length(&self) -> usize {
        let mut length = base_header_payload_length(&self.inner);

        if let Some(timestamp_millis_part) = self.timestamp_millis_part {
            length += timestamp_millis_part.length();
        }

        length
    }
}

impl From<Header> for BaseHeader {
    fn from(inner: Header) -> Self {
        Self { inner, timestamp_millis_part: None }
    }
}

impl AsRef<Header> for BaseHeader {
    fn as_ref(&self) -> &Header {
        &self.inner
    }
}

impl Sealable for BaseHeader {
    fn hash_slow(&self) -> B256 {
        Self::hash_slow(self)
    }
}

impl Encodable for BaseHeader {
    fn encode(&self, out: &mut dyn BufMut) {
        let list_header =
            alloy_rlp::Header { list: true, payload_length: self.header_payload_length() };
        list_header.encode(out);
        encode_inner_header(&self.inner, out);

        if let Some(timestamp_millis_part) = self.timestamp_millis_part {
            timestamp_millis_part.encode(out);
        }
    }

    fn length(&self) -> usize {
        let length = self.header_payload_length();
        length + length_of_length(length)
    }
}

fn base_header_payload_length(header: &Header) -> usize {
    let mut length = 0;
    length += header.parent_hash.length();
    length += header.ommers_hash.length();
    length += header.beneficiary.length();
    length += header.state_root.length();
    length += header.transactions_root.length();
    length += header.receipts_root.length();
    length += header.logs_bloom.length();
    length += header.difficulty.length();
    length += U256::from(header.number).length();
    length += U256::from(header.gas_limit).length();
    length += U256::from(header.gas_used).length();
    length += header.timestamp.length();
    length += header.extra_data.length();
    length += header.mix_hash.length();
    length += header.nonce.length();

    if let Some(base_fee) = header.base_fee_per_gas {
        length += U256::from(base_fee).length();
    }

    if let Some(root) = header.withdrawals_root {
        length += root.length();
    }

    if let Some(blob_gas_used) = header.blob_gas_used {
        length += U256::from(blob_gas_used).length();
    }

    if let Some(excess_blob_gas) = header.excess_blob_gas {
        length += U256::from(excess_blob_gas).length();
    }

    if let Some(parent_beacon_block_root) = header.parent_beacon_block_root {
        length += parent_beacon_block_root.length();
    }

    if let Some(requests_hash) = header.requests_hash {
        length += requests_hash.length();
    }

    if let Some(block_access_list_hash) = header.block_access_list_hash {
        length += block_access_list_hash.length();
    }

    if let Some(slot_number) = header.slot_number {
        length += U256::from(slot_number).length();
    }

    length
}

fn encode_inner_header(header: &Header, out: &mut dyn BufMut) {
    header.parent_hash.encode(out);
    header.ommers_hash.encode(out);
    header.beneficiary.encode(out);
    header.state_root.encode(out);
    header.transactions_root.encode(out);
    header.receipts_root.encode(out);
    header.logs_bloom.encode(out);
    header.difficulty.encode(out);
    U256::from(header.number).encode(out);
    U256::from(header.gas_limit).encode(out);
    U256::from(header.gas_used).encode(out);
    header.timestamp.encode(out);
    header.extra_data.encode(out);
    header.mix_hash.encode(out);
    header.nonce.encode(out);

    if let Some(base_fee) = header.base_fee_per_gas {
        U256::from(base_fee).encode(out);
    }

    if let Some(root) = header.withdrawals_root {
        root.encode(out);
    }

    if let Some(blob_gas_used) = header.blob_gas_used {
        U256::from(blob_gas_used).encode(out);
    }

    if let Some(excess_blob_gas) = header.excess_blob_gas {
        U256::from(excess_blob_gas).encode(out);
    }

    if let Some(parent_beacon_block_root) = header.parent_beacon_block_root {
        parent_beacon_block_root.encode(out);
    }

    if let Some(requests_hash) = header.requests_hash {
        requests_hash.encode(out);
    }

    if let Some(block_access_list_hash) = header.block_access_list_hash {
        block_access_list_hash.encode(out);
    }

    if let Some(slot_number) = header.slot_number {
        U256::from(slot_number).encode(out);
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_rlp::Encodable;

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
    fn timestamp_millis_is_absent_before_beryl() {
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
}
