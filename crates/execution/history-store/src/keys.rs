//! `SlateDB` index keys and values (TDD §2.3).
//!
//! | key | value |
//! | --- | --- |
//! | `t ‖ tx_hash` | block u64 BE ‖ index u32 BE |
//! | `b ‖ block_hash` | block u64 BE |
//! | `h ‖ hashed_key ‖ segment u64 BE` | roaring bitmap of block offsets within the segment |
//! | `m ‖ segment u64 BE` | JSON segment checksum manifest |
//! | `w` | publication watermark: last block u64 BE whose entries are written |
//!
//! `hashed_key` is a one-byte kind tag followed by reth's hashed-state keys:
//! `0x00 ‖ keccak(address)` for accounts and `0x01 ‖ keccak(address) ‖ keccak(slot)` for storage.
//! Big-endian integers keep byte order equal to numeric order, so a forward seek finds the next
//! segment for the same key.

use alloy_primitives::{Address, B256, keccak256};
use roaring::RoaringBitmap;

use crate::HistoryStoreError;

/// Tag byte of transaction-hash keys.
pub const TX_TAG: u8 = b't';
/// Tag byte of block-hash keys.
pub const BLOCK_TAG: u8 = b'b';
/// Tag byte of history keys.
pub const HISTORY_TAG: u8 = b'h';
/// Tag byte of segment manifest keys.
pub const MANIFEST_TAG: u8 = b'm';
/// Key of the index publication watermark.
pub const WATERMARK_KEY: &[u8] = b"w";
/// Kind byte of account hashed keys.
pub const ACCOUNT_KIND: u8 = 0x00;
/// Kind byte of storage hashed keys.
pub const STORAGE_KIND: u8 = 0x01;

/// A state key in hashed form: account or storage slot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HashedKey(pub Vec<u8>);

impl HashedKey {
    /// Hashed key of an account.
    pub fn account(address: Address) -> Self {
        let mut key = Vec::with_capacity(33);
        key.push(ACCOUNT_KIND);
        key.extend_from_slice(keccak256(address).as_slice());
        Self(key)
    }

    /// Hashed key of a storage slot.
    pub fn storage(address: Address, slot: B256) -> Self {
        let mut key = Vec::with_capacity(65);
        key.push(STORAGE_KIND);
        key.extend_from_slice(keccak256(address).as_slice());
        key.extend_from_slice(keccak256(slot).as_slice());
        Self(key)
    }
}

/// Encodes and decodes index keys and values.
#[derive(Debug, Clone, Copy)]
pub struct IndexKey;

impl IndexKey {
    /// `t ‖ tx_hash`.
    pub fn tx(hash: B256) -> Vec<u8> {
        Self::tagged(TX_TAG, hash.as_slice())
    }

    /// `b ‖ block_hash`.
    pub fn block(hash: B256) -> Vec<u8> {
        Self::tagged(BLOCK_TAG, hash.as_slice())
    }

    /// `h ‖ hashed_key` — the prefix shared by every segment entry of one key.
    pub fn history_prefix(key: &HashedKey) -> Vec<u8> {
        Self::tagged(HISTORY_TAG, &key.0)
    }

    /// `h ‖ hashed_key ‖ segment`.
    pub fn history(key: &HashedKey, segment: u64) -> Vec<u8> {
        let mut out = Self::history_prefix(key);
        out.extend_from_slice(&segment.to_be_bytes());
        out
    }

    /// `m ‖ segment`.
    pub fn manifest(segment: u64) -> Vec<u8> {
        Self::tagged(MANIFEST_TAG, &segment.to_be_bytes())
    }

    /// Reads the segment number from the tail of a history key.
    pub fn history_segment(key: &[u8]) -> Option<u64> {
        let tail = key.len().checked_sub(8)?;
        Some(u64::from_be_bytes(key[tail..].try_into().ok()?))
    }

    /// Encodes a transaction location value.
    pub fn tx_value(block: u64, index: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&block.to_be_bytes());
        out.extend_from_slice(&index.to_be_bytes());
        out
    }

    /// Decodes a transaction location value.
    pub fn decode_tx_value(raw: &[u8]) -> Result<(u64, u32), HistoryStoreError> {
        if raw.len() != 12 {
            return Err(Self::bad_value("tx location", raw.len()));
        }
        Ok((
            u64::from_be_bytes(raw[..8].try_into().expect("8 bytes")),
            u32::from_be_bytes(raw[8..].try_into().expect("4 bytes")),
        ))
    }

    /// Encodes a block number value.
    pub fn block_value(block: u64) -> Vec<u8> {
        block.to_be_bytes().to_vec()
    }

    /// Decodes a block number value.
    pub fn decode_block_value(raw: &[u8]) -> Result<u64, HistoryStoreError> {
        let bytes: [u8; 8] = raw.try_into().map_err(|_| Self::bad_value("block", raw.len()))?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Encodes a history bitmap value.
    pub fn bitmap_value(bitmap: &RoaringBitmap) -> Vec<u8> {
        let mut out = Vec::with_capacity(bitmap.serialized_size());
        bitmap.serialize_into(&mut out).expect("writing to a Vec cannot fail");
        out
    }

    /// Decodes a history bitmap value.
    pub fn decode_bitmap_value(raw: &[u8]) -> Result<RoaringBitmap, HistoryStoreError> {
        RoaringBitmap::deserialize_from(raw).map_err(|err| HistoryStoreError::Index {
            reason: format!("corrupt history bitmap: {err}"),
        })
    }

    /// Prepends a tag byte.
    pub fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(tag);
        out.extend_from_slice(body);
        out
    }

    /// Builds the error for a value of the wrong length.
    pub fn bad_value(what: &str, len: usize) -> HistoryStoreError {
        HistoryStoreError::Index { reason: format!("corrupt {what} value of {len} bytes") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values round trip and history keys sort by segment number.
    #[test]
    fn keys_round_trip_and_sort_numerically() {
        assert_eq!(
            IndexKey::decode_tx_value(&IndexKey::tx_value(7, 3)).unwrap(),
            (7, 3),
            "tx value round trips"
        );
        assert_eq!(
            IndexKey::decode_block_value(&IndexKey::block_value(u64::MAX - 1)).unwrap(),
            u64::MAX - 1,
            "block value round trips"
        );
        let key = HashedKey::account(Address::with_last_byte(1));
        assert_eq!(key.0.len(), 33, "account hashed key is tag + 32 bytes");
        assert_eq!(
            HashedKey::storage(Address::with_last_byte(1), B256::ZERO).0.len(),
            65,
            "storage hashed key is tag + 64 bytes"
        );
        let low = IndexKey::history(&key, 255);
        let high = IndexKey::history(&key, 256);
        assert!(low < high, "segment 255 sorts before segment 256");
        assert_eq!(IndexKey::history_segment(&high), Some(256), "segment parses back");
        let mut bitmap = RoaringBitmap::new();
        bitmap.insert(0);
        bitmap.insert(7);
        assert_eq!(
            IndexKey::decode_bitmap_value(&IndexKey::bitmap_value(&bitmap)).unwrap(),
            bitmap,
            "bitmap round trips"
        );
    }
}
