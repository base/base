//! V2 proof storage table keys.

use alloy_primitives::{B256, BlockNumber};
use reth_db::{
    DatabaseError,
    models::sharded_key::ShardedKey,
    table::{Decode, Encode},
};
use reth_trie_common::{Nibbles, StoredNibbles};
use serde::{Deserialize, Serialize};

const NIBBLE_SUBKEY_LEN: usize = 65;
const ACCOUNT_TRIE_SHARDED_KEY_LEN: usize = NIBBLE_SUBKEY_LEN + 8;
const STORAGE_TRIE_SHARDED_KEY_LEN: usize = 32 + NIBBLE_SUBKEY_LEN + 8;

fn encode_nibble_subkey(nibbles: &StoredNibbles) -> [u8; NIBBLE_SUBKEY_LEN] {
    assert!(nibbles.0.len() <= 64, "nibble path exceeds 64 nibbles");
    let mut buf = [0u8; NIBBLE_SUBKEY_LEN];
    for (index, nibble) in nibbles.0.iter().enumerate() {
        buf[index] = nibble;
    }
    buf[64] = nibbles.0.len() as u8;
    buf
}

fn decode_nibble_subkey(buf: &[u8; NIBBLE_SUBKEY_LEN]) -> Result<StoredNibbles, DatabaseError> {
    let len = buf[64] as usize;
    if len > 64 {
        return Err(DatabaseError::Decode);
    }
    Ok(StoredNibbles::from(Nibbles::from_nibbles_unchecked(&buf[..len])))
}

/// Sharded key for V2 hashed account history.
#[derive(Debug, Default, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Hash)]
pub struct HashedAccountShardedKey(pub ShardedKey<B256>);

impl HashedAccountShardedKey {
    /// Creates a sharded key for `key` with `highest_block_number`.
    pub const fn new(key: B256, highest_block_number: u64) -> Self {
        Self(ShardedKey::new(key, highest_block_number))
    }
}

impl Encode for HashedAccountShardedKey {
    type Encoded = [u8; 40];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; 40];
        buf[..32].copy_from_slice(self.0.key.as_slice());
        buf[32..].copy_from_slice(&self.0.highest_block_number.to_be_bytes());
        buf
    }
}

impl Decode for HashedAccountShardedKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 40 {
            return Err(DatabaseError::Decode);
        }
        let key = B256::from_slice(&value[..32]);
        let highest_block_number =
            u64::from_be_bytes(value[32..].try_into().map_err(|_| DatabaseError::Decode)?);
        Ok(Self::new(key, highest_block_number))
    }
}

/// Sharded key for V2 hashed storage history.
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct HashedStorageShardedKey {
    /// Hashed account address.
    pub hashed_address: B256,
    /// Storage key and shard block.
    pub sharded_key: ShardedKey<B256>,
}

impl HashedStorageShardedKey {
    /// Creates a sharded key for `hashed_address`, `key`, and `highest_block_number`.
    pub const fn new(hashed_address: B256, key: B256, highest_block_number: u64) -> Self {
        Self { hashed_address, sharded_key: ShardedKey::new(key, highest_block_number) }
    }
}

impl Encode for HashedStorageShardedKey {
    type Encoded = [u8; 72];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; 72];
        buf[..32].copy_from_slice(self.hashed_address.as_slice());
        buf[32..64].copy_from_slice(self.sharded_key.key.as_slice());
        buf[64..].copy_from_slice(&self.sharded_key.highest_block_number.to_be_bytes());
        buf
    }
}

impl Decode for HashedStorageShardedKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 72 {
            return Err(DatabaseError::Decode);
        }
        let hashed_address = B256::from_slice(&value[..32]);
        let key = B256::from_slice(&value[32..64]);
        let highest_block_number =
            u64::from_be_bytes(value[64..].try_into().map_err(|_| DatabaseError::Decode)?);
        Ok(Self::new(hashed_address, key, highest_block_number))
    }
}

/// Sharded key for V2 account trie history.
#[derive(Debug, Default, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Hash)]
pub struct AccountTrieShardedKey {
    /// Trie path.
    pub key: StoredNibbles,
    /// Highest block number in this shard.
    pub highest_block_number: u64,
}

impl AccountTrieShardedKey {
    /// Creates a sharded account trie key.
    pub const fn new(key: StoredNibbles, highest_block_number: u64) -> Self {
        Self { key, highest_block_number }
    }
}

impl Encode for AccountTrieShardedKey {
    type Encoded = [u8; ACCOUNT_TRIE_SHARDED_KEY_LEN];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; ACCOUNT_TRIE_SHARDED_KEY_LEN];
        buf[..NIBBLE_SUBKEY_LEN].copy_from_slice(&encode_nibble_subkey(&self.key));
        buf[NIBBLE_SUBKEY_LEN..].copy_from_slice(&self.highest_block_number.to_be_bytes());
        buf
    }
}

impl Decode for AccountTrieShardedKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        let bytes: &[u8; ACCOUNT_TRIE_SHARDED_KEY_LEN] =
            value.try_into().map_err(|_| DatabaseError::Decode)?;
        let nibble_buf: &[u8; NIBBLE_SUBKEY_LEN] =
            bytes[..NIBBLE_SUBKEY_LEN].try_into().map_err(|_| DatabaseError::Decode)?;
        let key = decode_nibble_subkey(nibble_buf)?;
        let highest_block_number = u64::from_be_bytes(
            bytes[NIBBLE_SUBKEY_LEN..].try_into().map_err(|_| DatabaseError::Decode)?,
        );
        Ok(Self { key, highest_block_number })
    }
}

/// Sharded key for V2 storage trie history.
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct StorageTrieShardedKey {
    /// Hashed account address.
    pub hashed_address: B256,
    /// Trie path.
    pub key: StoredNibbles,
    /// Highest block number in this shard.
    pub highest_block_number: u64,
}

impl StorageTrieShardedKey {
    /// Creates a sharded storage trie key.
    pub const fn new(hashed_address: B256, key: StoredNibbles, highest_block_number: u64) -> Self {
        Self { hashed_address, key, highest_block_number }
    }
}

impl Encode for StorageTrieShardedKey {
    type Encoded = [u8; STORAGE_TRIE_SHARDED_KEY_LEN];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; STORAGE_TRIE_SHARDED_KEY_LEN];
        buf[..32].copy_from_slice(self.hashed_address.as_slice());
        buf[32..32 + NIBBLE_SUBKEY_LEN].copy_from_slice(&encode_nibble_subkey(&self.key));
        buf[32 + NIBBLE_SUBKEY_LEN..].copy_from_slice(&self.highest_block_number.to_be_bytes());
        buf
    }
}

impl Decode for StorageTrieShardedKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        let bytes: &[u8; STORAGE_TRIE_SHARDED_KEY_LEN] =
            value.try_into().map_err(|_| DatabaseError::Decode)?;
        let hashed_address = B256::from_slice(&bytes[..32]);
        let nibble_buf: &[u8; NIBBLE_SUBKEY_LEN] =
            bytes[32..32 + NIBBLE_SUBKEY_LEN].try_into().map_err(|_| DatabaseError::Decode)?;
        let key = decode_nibble_subkey(nibble_buf)?;
        let highest_block_number = u64::from_be_bytes(
            bytes[32 + NIBBLE_SUBKEY_LEN..].try_into().map_err(|_| DatabaseError::Decode)?,
        );
        Ok(Self { hashed_address, key, highest_block_number })
    }
}

/// Key for V2 storage changesets grouped by block and hashed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct BlockNumberHashedAddress(pub (BlockNumber, B256));

impl Encode for BlockNumberHashedAddress {
    type Encoded = [u8; 40];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; 40];
        buf[..8].copy_from_slice(&self.0.0.to_be_bytes());
        buf[8..].copy_from_slice(self.0.1.as_slice());
        buf
    }
}

impl Decode for BlockNumberHashedAddress {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 40 {
            return Err(DatabaseError::Decode);
        }
        let block_number =
            u64::from_be_bytes(value[..8].try_into().map_err(|_| DatabaseError::Decode)?);
        let hashed_address = B256::from_slice(&value[8..]);
        Ok(Self((block_number, hashed_address)))
    }
}
