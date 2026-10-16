//! Per-block validity classification record and its versioned binary encoding.

use alloy_primitives::{B256, TxHash};
use thiserror::Error;

/// Current on-disk record format version.
pub const VALIDITY_RECORD_VERSION: u8 = 1;

/// Maximum number of validity transaction hashes stored in one block record.
///
/// Bounds the encoded record to [`MAX_VALIDITY_RECORD_LEN`] bytes, which in turn bounds the
/// disk read and decode work performed by a single status lookup.
pub const MAX_VALIDITY_TRANSACTIONS_PER_RECORD: usize = 65_536;

/// Length of the fixed record header: version, block hash, block number, transaction count and
/// validity transaction count.
pub const VALIDITY_RECORD_HEADER_LEN: usize = 1 + 32 + 8 + 4 + 4;

/// Maximum length of an encoded record.
pub const MAX_VALIDITY_RECORD_LEN: usize =
    VALIDITY_RECORD_HEADER_LEN + MAX_VALIDITY_TRANSACTIONS_PER_RECORD * 32;

/// Reasons an encoded record or a record about to be encoded is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidityRecordError {
    /// The encoded record is shorter than the fixed header.
    #[error("record is truncated: {len} bytes is shorter than the header")]
    Truncated {
        /// Encoded length.
        len: usize,
    },
    /// The encoded record uses an unknown format version.
    #[error("unsupported record version {version}")]
    UnsupportedVersion {
        /// Version byte found in the record.
        version: u8,
    },
    /// The encoded length does not match the declared validity transaction count.
    #[error("record length {actual} does not match declared length {expected}")]
    LengthMismatch {
        /// Length implied by the declared validity transaction count.
        expected: usize,
        /// Encoded length.
        actual: usize,
    },
    /// The record holds more validity transactions than [`MAX_VALIDITY_TRANSACTIONS_PER_RECORD`].
    #[error("record holds {count} validity transactions, above the limit of {max}")]
    TooManyValidityTransactions {
        /// Number of validity transactions.
        count: usize,
        /// Maximum supported count.
        max: usize,
    },
    /// The record holds more validity transactions than the block holds transactions.
    #[error("record holds {validity} validity transactions but the block holds {transactions}")]
    ExceedsTransactionCount {
        /// Number of validity transactions.
        validity: usize,
        /// Number of transactions in the block.
        transactions: u32,
    },
    /// Validity transaction hashes are not strictly ascending.
    #[error("validity transaction hashes are not strictly ascending")]
    Unsorted,
    /// The record's block hash differs from the key it is stored under.
    #[error("record block hash does not match its storage key")]
    KeyMismatch,
}

/// Validity classification of every transaction in one sealed, pool-built block.
///
/// A record exists for every recorded pool-built block, including blocks without validity
/// transactions, so that an absent record (unknown) is distinguishable from a recorded block in
/// which a transaction was not a validity transaction (`false`).
///
/// `validity_transactions` must be strictly ascending for [`Self::contains`] and encoding;
/// [`Self::new`] and decoded records satisfy this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidityBlockRecord {
    /// Hash of the sealed block.
    pub block_hash: B256,
    /// Number of the sealed block.
    pub block_number: u64,
    /// Number of transactions in the sealed block, including forced and attribute transactions.
    pub transaction_count: u32,
    /// Hashes of committed pool transactions that carried nonempty validity predicates.
    pub validity_transactions: Vec<TxHash>,
}

impl ValidityBlockRecord {
    /// Creates a record, sorting and de-duplicating the validity transaction hashes.
    pub fn new(
        block_hash: B256,
        block_number: u64,
        transaction_count: u32,
        mut validity_transactions: Vec<TxHash>,
    ) -> Self {
        validity_transactions.sort_unstable();
        validity_transactions.dedup();
        Self { block_hash, block_number, transaction_count, validity_transactions }
    }

    /// Returns whether `tx_hash` was classified as a validity transaction in this block.
    pub fn contains(&self, tx_hash: &TxHash) -> bool {
        self.validity_transactions.binary_search(tx_hash).is_ok()
    }

    /// Checks the bounds and ordering invariants required for encoding.
    pub fn validate(&self) -> Result<(), ValidityRecordError> {
        let count = self.validity_transactions.len();
        if count > MAX_VALIDITY_TRANSACTIONS_PER_RECORD {
            return Err(ValidityRecordError::TooManyValidityTransactions {
                count,
                max: MAX_VALIDITY_TRANSACTIONS_PER_RECORD,
            });
        }
        if count > self.transaction_count as usize {
            return Err(ValidityRecordError::ExceedsTransactionCount {
                validity: count,
                transactions: self.transaction_count,
            });
        }
        if self.validity_transactions.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ValidityRecordError::Unsorted);
        }
        Ok(())
    }

    /// Encodes the record in format version [`VALIDITY_RECORD_VERSION`].
    ///
    /// Layout, with integers big-endian:
    ///
    /// ```text
    /// [ version (1) | block hash (32) | block number (8) | transaction count (4)
    ///   | validity count (4) | validity hashes (32 * validity count) ]
    /// ```
    pub fn encode(&self) -> Result<Vec<u8>, ValidityRecordError> {
        self.validate()?;
        let count = self.validity_transactions.len();
        let mut out = Vec::with_capacity(VALIDITY_RECORD_HEADER_LEN + count * 32);
        out.push(VALIDITY_RECORD_VERSION);
        out.extend_from_slice(self.block_hash.as_slice());
        out.extend_from_slice(&self.block_number.to_be_bytes());
        out.extend_from_slice(&self.transaction_count.to_be_bytes());
        // `validate` bounds the count by `MAX_VALIDITY_TRANSACTIONS_PER_RECORD`, so it fits.
        out.extend_from_slice(&(count as u32).to_be_bytes());
        for hash in &self.validity_transactions {
            out.extend_from_slice(hash.as_slice());
        }
        Ok(out)
    }

    /// Decodes and validates a record encoded by [`Self::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ValidityRecordError> {
        let truncated = ValidityRecordError::Truncated { len: bytes.len() };
        if bytes.len() < VALIDITY_RECORD_HEADER_LEN {
            return Err(truncated);
        }
        let Some((&[version], rest)) = bytes.split_first_chunk::<1>() else {
            return Err(truncated);
        };
        if version != VALIDITY_RECORD_VERSION {
            return Err(ValidityRecordError::UnsupportedVersion { version });
        }
        let Some((block_hash, rest)) = rest.split_first_chunk::<32>() else {
            return Err(truncated);
        };
        let Some((block_number, rest)) = rest.split_first_chunk::<8>() else {
            return Err(truncated);
        };
        let Some((transaction_count, rest)) = rest.split_first_chunk::<4>() else {
            return Err(truncated);
        };
        let Some((count, hashes)) = rest.split_first_chunk::<4>() else {
            return Err(truncated);
        };
        let block_hash = B256::from(*block_hash);
        let block_number = u64::from_be_bytes(*block_number);
        let transaction_count = u32::from_be_bytes(*transaction_count);
        let count = u32::from_be_bytes(*count) as usize;
        if count > MAX_VALIDITY_TRANSACTIONS_PER_RECORD {
            return Err(ValidityRecordError::TooManyValidityTransactions {
                count,
                max: MAX_VALIDITY_TRANSACTIONS_PER_RECORD,
            });
        }
        let expected = VALIDITY_RECORD_HEADER_LEN + count * 32;
        if bytes.len() != expected {
            return Err(ValidityRecordError::LengthMismatch { expected, actual: bytes.len() });
        }
        let validity_transactions = hashes.chunks_exact(32).map(TxHash::from_slice).collect();
        let record = Self { block_hash, block_number, transaction_count, validity_transactions };
        record.validate()?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::b256;

    use super::*;

    const BLOCK: B256 = b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const TX_A: B256 = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const TX_B: B256 = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    fn header(version: u8, transaction_count: u32, validity_count: u32) -> Vec<u8> {
        let mut out = vec![version];
        out.extend_from_slice(BLOCK.as_slice());
        out.extend_from_slice(&7u64.to_be_bytes());
        out.extend_from_slice(&transaction_count.to_be_bytes());
        out.extend_from_slice(&validity_count.to_be_bytes());
        out
    }

    #[test]
    fn encoding_matches_documented_layout() {
        let record = ValidityBlockRecord::new(BLOCK, 7, 3, vec![TX_B, TX_A, TX_B]);
        let mut expected = header(1, 3, 2);
        expected.extend_from_slice(TX_A.as_slice());
        expected.extend_from_slice(TX_B.as_slice());

        let encoded = record.encode().unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(encoded.len(), 49 + 64);
        assert_eq!(ValidityBlockRecord::decode(&encoded).unwrap(), record);
    }

    #[test]
    fn empty_record_round_trips_and_classifies_false() {
        let record = ValidityBlockRecord::new(BLOCK, 7, 5, vec![]);
        let encoded = record.encode().unwrap();
        assert_eq!(encoded, header(1, 5, 0));
        let decoded = ValidityBlockRecord::decode(&encoded).unwrap();
        assert!(!decoded.contains(&TX_A));
        assert_eq!(decoded.transaction_count, 5);
    }

    #[test]
    fn contains_matches_membership() {
        let record = ValidityBlockRecord::new(BLOCK, 7, 10, vec![TX_B, TX_A]);
        assert!(record.contains(&TX_A));
        assert!(record.contains(&TX_B));
        assert!(!record.contains(&BLOCK));
    }

    #[test]
    fn decode_rejects_truncated_header() {
        let bytes = header(1, 1, 0);
        assert_eq!(
            ValidityBlockRecord::decode(&bytes[..48]),
            Err(ValidityRecordError::Truncated { len: 48 })
        );
        assert_eq!(
            ValidityBlockRecord::decode(&[]),
            Err(ValidityRecordError::Truncated { len: 0 })
        );
    }

    #[test]
    fn decode_rejects_unknown_version() {
        for version in [0, 2, u8::MAX] {
            assert_eq!(
                ValidityBlockRecord::decode(&header(version, 1, 0)),
                Err(ValidityRecordError::UnsupportedVersion { version })
            );
        }
    }

    #[test]
    fn decode_rejects_truncated_and_trailing_hashes() {
        let mut bytes = header(1, 2, 1);
        bytes.extend_from_slice(&TX_A[..31]);
        assert_eq!(
            ValidityBlockRecord::decode(&bytes),
            Err(ValidityRecordError::LengthMismatch { expected: 81, actual: 80 })
        );
        let mut bytes = header(1, 2, 0);
        bytes.extend_from_slice(TX_A.as_slice());
        assert_eq!(
            ValidityBlockRecord::decode(&bytes),
            Err(ValidityRecordError::LengthMismatch { expected: 49, actual: 81 })
        );
    }

    #[test]
    fn decode_rejects_count_above_limit_before_reading_hashes() {
        let count = MAX_VALIDITY_TRANSACTIONS_PER_RECORD as u32 + 1;
        assert_eq!(
            ValidityBlockRecord::decode(&header(1, u32::MAX, count)),
            Err(ValidityRecordError::TooManyValidityTransactions {
                count: count as usize,
                max: MAX_VALIDITY_TRANSACTIONS_PER_RECORD,
            })
        );
        assert_eq!(
            ValidityBlockRecord::decode(&header(1, u32::MAX, u32::MAX)),
            Err(ValidityRecordError::TooManyValidityTransactions {
                count: u32::MAX as usize,
                max: MAX_VALIDITY_TRANSACTIONS_PER_RECORD,
            })
        );
    }

    #[test]
    fn decode_rejects_more_validity_transactions_than_block_transactions() {
        let mut bytes = header(1, 1, 2);
        bytes.extend_from_slice(TX_A.as_slice());
        bytes.extend_from_slice(TX_B.as_slice());
        assert_eq!(
            ValidityBlockRecord::decode(&bytes),
            Err(ValidityRecordError::ExceedsTransactionCount { validity: 2, transactions: 1 })
        );
    }

    #[test]
    fn decode_rejects_unsorted_or_duplicate_hashes() {
        for (first, second) in [(TX_B, TX_A), (TX_A, TX_A)] {
            let mut bytes = header(1, 2, 2);
            bytes.extend_from_slice(first.as_slice());
            bytes.extend_from_slice(second.as_slice());
            assert_eq!(ValidityBlockRecord::decode(&bytes), Err(ValidityRecordError::Unsorted));
        }
    }

    #[test]
    fn encode_rejects_invalid_records() {
        let unsorted = ValidityBlockRecord {
            block_hash: BLOCK,
            block_number: 7,
            transaction_count: 2,
            validity_transactions: vec![TX_B, TX_A],
        };
        assert_eq!(unsorted.encode(), Err(ValidityRecordError::Unsorted));

        let too_few_transactions = ValidityBlockRecord::new(BLOCK, 7, 1, vec![TX_A, TX_B]);
        assert_eq!(
            too_few_transactions.encode(),
            Err(ValidityRecordError::ExceedsTransactionCount { validity: 2, transactions: 1 })
        );
    }

    #[test]
    fn record_size_limit_is_inclusive() {
        let hashes = |count: usize| -> Vec<TxHash> {
            (0..count as u64).map(|i| B256::left_padding_from(&i.to_be_bytes())).collect()
        };
        let at_limit = ValidityBlockRecord::new(
            BLOCK,
            7,
            u32::MAX,
            hashes(MAX_VALIDITY_TRANSACTIONS_PER_RECORD),
        );
        let encoded = at_limit.encode().unwrap();
        assert_eq!(encoded.len(), MAX_VALIDITY_RECORD_LEN);
        assert_eq!(ValidityBlockRecord::decode(&encoded).unwrap(), at_limit);

        let over_limit = ValidityBlockRecord::new(
            BLOCK,
            7,
            u32::MAX,
            hashes(MAX_VALIDITY_TRANSACTIONS_PER_RECORD + 1),
        );
        assert_eq!(
            over_limit.encode(),
            Err(ValidityRecordError::TooManyValidityTransactions {
                count: MAX_VALIDITY_TRANSACTIONS_PER_RECORD + 1,
                max: MAX_VALIDITY_TRANSACTIONS_PER_RECORD,
            })
        );
    }
}
