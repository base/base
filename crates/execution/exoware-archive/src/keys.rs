//! Key layout: `gen:u8 ‖ family:u8 ‖ fields ‖ (seg|n):u64be`.
//!
//! Every key is fixed width per family. Index families (`0x01`–`0x05`) end in the segment
//! number; block families (`0x10`–`0x15`) end in the block number; the marker ends in the
//! segment number. The trailing big-endian number gives time-ordered range scans per key and a
//! capture group for server-side pruning.

use alloy_primitives::{Address, B256, keccak256};
use bytes::Bytes;

/// Format generation, the first byte of every key. A format change is a new generation and
/// therefore a disjoint key space.
pub const FORMAT_VERSION: u8 = 1;

/// Width of the trailing segment or block number.
pub const NUMBER_LEN: usize = 8;

/// Key family, the second byte of every key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// `gen‖00`: archive config row.
    Config,
    /// `gen‖01‖block_hash‖seg` → block number.
    BlockHash,
    /// `gen‖02‖tx_hash‖seg` → `(block, index)`.
    TxHash,
    /// `gen‖03‖kind‖hashed_key‖seg` → offset set of change blocks.
    StateHistory,
    /// `gen‖04‖address‖seg` → offset set of blocks with a log from the address.
    LogAddress,
    /// `gen‖05‖position‖topic‖seg` → offset set of blocks with the topic at that position.
    LogTopic,
    /// `gen‖10‖n` → RLP header.
    Header,
    /// `gen‖11‖n` → transactions and withdrawals.
    Body,
    /// `gen‖12‖n` → receipts.
    Receipts,
    /// `gen‖13‖n` → senders.
    Senders,
    /// `gen‖14‖n` → account before-values.
    AccountChanges,
    /// `gen‖15‖n` → storage before-values.
    StorageChanges,
    /// `gen‖1F‖seg` → segment marker.
    Marker,
}

impl Family {
    /// Index families, keyed by a lookup field and the trailing segment.
    pub const INDEX: [Self; 5] =
        [Self::BlockHash, Self::TxHash, Self::StateHistory, Self::LogAddress, Self::LogTopic];

    /// Block families, keyed by block number.
    pub const BLOCK: [Self; 6] = [
        Self::Header,
        Self::Body,
        Self::Receipts,
        Self::Senders,
        Self::AccountChanges,
        Self::StorageChanges,
    ];

    /// Family tag byte.
    pub const fn byte(self) -> u8 {
        match self {
            Self::Config => 0x00,
            Self::BlockHash => 0x01,
            Self::TxHash => 0x02,
            Self::StateHistory => 0x03,
            Self::LogAddress => 0x04,
            Self::LogTopic => 0x05,
            Self::Header => 0x10,
            Self::Body => 0x11,
            Self::Receipts => 0x12,
            Self::Senders => 0x13,
            Self::AccountChanges => 0x14,
            Self::StorageChanges => 0x15,
            Self::Marker => 0x1f,
        }
    }

    /// Parses a family tag byte.
    pub const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x00 => Self::Config,
            0x01 => Self::BlockHash,
            0x02 => Self::TxHash,
            0x03 => Self::StateHistory,
            0x04 => Self::LogAddress,
            0x05 => Self::LogTopic,
            0x10 => Self::Header,
            0x11 => Self::Body,
            0x12 => Self::Receipts,
            0x13 => Self::Senders,
            0x14 => Self::AccountChanges,
            0x15 => Self::StorageChanges,
            0x1f => Self::Marker,
            _ => return None,
        })
    }

    /// Width of the lookup fields between the family byte and the trailing number.
    pub const fn fields_len(self) -> usize {
        match self {
            Self::BlockHash | Self::TxHash => 32,
            Self::StateHistory | Self::LogTopic => 33,
            Self::LogAddress => 20,
            Self::Config
            | Self::Header
            | Self::Body
            | Self::Receipts
            | Self::Senders
            | Self::AccountChanges
            | Self::StorageChanges
            | Self::Marker => 0,
        }
    }

    /// Total key width.
    pub const fn key_len(self) -> usize {
        match self {
            Self::Config => 2,
            _ => 2 + self.fields_len() + NUMBER_LEN,
        }
    }

    /// Whether rows of this family belong to a segment's digest (everything but config and
    /// marker).
    pub const fn is_segment_row(self) -> bool {
        !matches!(self, Self::Config | Self::Marker)
    }

    /// Whether the trailing number is a block number (otherwise a segment number).
    pub const fn keyed_by_block(self) -> bool {
        matches!(
            self,
            Self::Header
                | Self::Body
                | Self::Receipts
                | Self::Senders
                | Self::AccountChanges
                | Self::StorageChanges
        )
    }

    /// Metric label.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::BlockHash => "block_hash",
            Self::TxHash => "tx_hash",
            Self::StateHistory => "state_history",
            Self::LogAddress => "log_address",
            Self::LogTopic => "log_topic",
            Self::Header => "header",
            Self::Body => "body",
            Self::Receipts => "receipts",
            Self::Senders => "senders",
            Self::AccountChanges => "account_changes",
            Self::StorageChanges => "storage_changes",
            Self::Marker => "marker",
        }
    }
}

/// State-history key kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateKind {
    /// Account (`hashed_key = keccak(address)`).
    Account,
    /// Storage slot (`hashed_key = keccak(address ‖ slot)`).
    Storage,
}

impl StateKind {
    /// Kind byte.
    pub const fn byte(self) -> u8 {
        match self {
            Self::Account => 0,
            Self::Storage => 1,
        }
    }
}

/// A state-history lookup key: kind plus hashed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateKey {
    /// Account or storage.
    pub kind: StateKind,
    /// Hashed key.
    pub hashed: B256,
}

impl StateKey {
    /// Key of an account.
    pub fn account(address: Address) -> Self {
        Self { kind: StateKind::Account, hashed: keccak256(address) }
    }

    /// Key of a storage slot.
    pub fn storage(address: Address, slot: B256) -> Self {
        let mut buf = [0u8; 52];
        buf[..20].copy_from_slice(address.as_slice());
        buf[20..].copy_from_slice(slot.as_slice());
        Self { kind: StateKind::Storage, hashed: keccak256(buf) }
    }

    /// The 33 field bytes `kind ‖ hashed`.
    pub fn fields(&self) -> [u8; 33] {
        let mut out = [0u8; 33];
        out[0] = self.kind.byte();
        out[1..].copy_from_slice(self.hashed.as_slice());
        out
    }
}

/// Builds and parses archive keys.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArchiveKey;

impl ArchiveKey {
    /// `gen ‖ family`, the prefix of every key of `family`.
    pub fn family_prefix(family: Family) -> Bytes {
        Bytes::copy_from_slice(&[FORMAT_VERSION, family.byte()])
    }

    /// `gen ‖ family ‖ fields ‖ number`.
    pub fn build(family: Family, fields: &[u8], number: u64) -> Bytes {
        debug_assert_eq!(fields.len(), family.fields_len(), "field width of {family:?}");
        let mut key = Vec::with_capacity(2 + fields.len() + NUMBER_LEN);
        key.push(FORMAT_VERSION);
        key.push(family.byte());
        key.extend_from_slice(fields);
        key.extend_from_slice(&number.to_be_bytes());
        key.into()
    }

    /// Config row key.
    pub fn config() -> Bytes {
        Self::family_prefix(Family::Config)
    }

    /// Marker key of segment `segment`.
    pub fn marker(segment: u64) -> Bytes {
        Self::build(Family::Marker, &[], segment)
    }

    /// Block-family key of block `number`.
    pub fn block(family: Family, number: u64) -> Bytes {
        debug_assert!(family.keyed_by_block(), "{family:?} is not a block family");
        Self::build(family, &[], number)
    }

    /// Block-hash locator.
    pub fn block_hash(hash: B256, segment: u64) -> Bytes {
        Self::build(Family::BlockHash, hash.as_slice(), segment)
    }

    /// Transaction-hash locator.
    pub fn tx_hash(hash: B256, segment: u64) -> Bytes {
        Self::build(Family::TxHash, hash.as_slice(), segment)
    }

    /// State-history posting.
    pub fn state(key: &StateKey, segment: u64) -> Bytes {
        Self::build(Family::StateHistory, &key.fields(), segment)
    }

    /// Log-address posting.
    pub fn log_address(address: Address, segment: u64) -> Bytes {
        Self::build(Family::LogAddress, address.as_slice(), segment)
    }

    /// Log-topic posting for `topic` at `position` (0–3).
    pub fn log_topic(position: u8, topic: B256, segment: u64) -> Bytes {
        let mut fields = [0u8; 33];
        fields[0] = position;
        fields[1..].copy_from_slice(topic.as_slice());
        Self::build(Family::LogTopic, &fields, segment)
    }

    /// Family of a well-formed key, checking the generation byte and the family width.
    pub fn family(key: &[u8]) -> Option<Family> {
        if key.len() < 2 || key[0] != FORMAT_VERSION {
            return None;
        }
        let family = Family::from_byte(key[1])?;
        (key.len() == family.key_len()).then_some(family)
    }

    /// Trailing segment or block number of a well-formed key.
    pub fn number(key: &[u8]) -> Option<u64> {
        let family = Self::family(key)?;
        if family == Family::Config {
            return None;
        }
        let tail: [u8; NUMBER_LEN] = key[key.len() - NUMBER_LEN..].try_into().ok()?;
        Some(u64::from_be_bytes(tail))
    }

    /// Lookup fields of a well-formed key.
    pub fn fields(key: &[u8]) -> Option<&[u8]> {
        let family = Self::family(key)?;
        Some(&key[2..2 + family.fields_len()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every family has the documented width and the trailing number round-trips.
    #[test]
    fn key_widths_per_family() {
        let hash = B256::repeat_byte(0xab);
        let address = Address::repeat_byte(0xcd);
        let cases = [
            (ArchiveKey::config(), Family::Config, 2),
            (ArchiveKey::block_hash(hash, 7), Family::BlockHash, 42),
            (ArchiveKey::tx_hash(hash, 7), Family::TxHash, 42),
            (ArchiveKey::state(&StateKey::account(address), 7), Family::StateHistory, 43),
            (ArchiveKey::state(&StateKey::storage(address, hash), 7), Family::StateHistory, 43),
            (ArchiveKey::log_address(address, 7), Family::LogAddress, 30),
            (ArchiveKey::log_topic(3, hash, 7), Family::LogTopic, 43),
            (ArchiveKey::block(Family::Header, 7), Family::Header, 10),
            (ArchiveKey::block(Family::Body, 7), Family::Body, 10),
            (ArchiveKey::block(Family::Receipts, 7), Family::Receipts, 10),
            (ArchiveKey::block(Family::Senders, 7), Family::Senders, 10),
            (ArchiveKey::block(Family::AccountChanges, 7), Family::AccountChanges, 10),
            (ArchiveKey::block(Family::StorageChanges, 7), Family::StorageChanges, 10),
            (ArchiveKey::marker(7), Family::Marker, 10),
        ];
        for (key, family, len) in cases {
            assert_eq!(key.len(), len, "{family:?} key width");
            assert_eq!(family.key_len(), len, "{family:?} declared width");
            assert_eq!(key[0], FORMAT_VERSION, "{family:?} generation byte");
            assert_eq!(key[1], family.byte(), "{family:?} family byte");
            assert_eq!(ArchiveKey::family(&key), Some(family), "{family:?} parses back");
            if family != Family::Config {
                assert_eq!(ArchiveKey::number(&key), Some(7), "{family:?} trailing number");
            }
        }
        let state = ArchiveKey::state(&StateKey::storage(address, hash), 0);
        assert_eq!(state[2], 1, "storage kind byte");
        let topic = ArchiveKey::log_topic(3, hash, 0);
        assert_eq!(topic[2], 3, "topic position byte");
        assert_eq!(&topic[3..35], hash.as_slice(), "topic bytes follow the position");
    }

    /// Keys of one lookup field sort by segment, and block keys sort by number across the
    /// `u8` boundary of the big-endian encoding.
    #[test]
    fn keys_order_by_trailing_number() {
        let hash = B256::repeat_byte(1);
        assert!(ArchiveKey::tx_hash(hash, 255) < ArchiveKey::tx_hash(hash, 256), "segment order");
        assert!(
            ArchiveKey::block(Family::Header, 255) < ArchiveKey::block(Family::Header, 256),
            "block order"
        );
        assert!(ArchiveKey::marker(u64::MAX - 1) < ArchiveKey::marker(u64::MAX), "marker order");
        assert!(
            ArchiveKey::block(Family::StorageChanges, u64::MAX) < ArchiveKey::marker(0),
            "markers sort after all block rows"
        );
        assert!(
            ArchiveKey::log_topic(4, B256::ZERO, 0)
                > ArchiveKey::log_topic(3, B256::repeat_byte(0xff), u64::MAX),
            "topic position orders before topic"
        );
    }

    /// Malformed keys are rejected by the parser.
    #[test]
    fn malformed_keys_do_not_parse() {
        assert_eq!(
            ArchiveKey::family(&[FORMAT_VERSION + 1, 0x10, 0, 0, 0, 0, 0, 0, 0, 0]),
            None,
            "other generation"
        );
        assert_eq!(ArchiveKey::family(&[FORMAT_VERSION, 0x10, 0]), None, "short block key");
        assert_eq!(
            ArchiveKey::family(&[FORMAT_VERSION, 0x09, 0, 0, 0, 0, 0, 0, 0, 0]),
            None,
            "unknown family"
        );
    }
}
