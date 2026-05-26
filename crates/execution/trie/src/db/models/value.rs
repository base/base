//! V2 proof storage table values.

use alloy_primitives::B256;
use bytes::BufMut;
use reth_codecs::{Compact, DecompressError};
use reth_db::{
    DatabaseError,
    table::{Compress, Decompress},
};
use reth_primitives_traits::{Account, ValueWithSubKey};
use reth_trie_common::{BranchNodeCompact, StoredNibblesSubKey};
use serde::{Deserialize, Serialize};

use super::key::NIBBLE_SUBKEY_LEN;

/// Previous account value stored in V2 hashed account changesets.
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct HashedAccountBeforeTx {
    /// Hashed account address. Acts as the dup-sort subkey.
    pub hashed_address: B256,
    /// Account value before the block, or `None` if the account did not exist.
    pub info: Option<Account>,
}

impl HashedAccountBeforeTx {
    /// Creates a previous-account changeset value.
    pub const fn new(hashed_address: B256, info: Option<Account>) -> Self {
        Self { hashed_address, info }
    }
}

impl ValueWithSubKey for HashedAccountBeforeTx {
    type SubKey = B256;

    fn get_subkey(&self) -> Self::SubKey {
        self.hashed_address
    }
}

impl Compress for HashedAccountBeforeTx {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        buf.put_slice(self.hashed_address.as_slice());
        if let Some(account) = &self.info {
            account.compress_to_buf(buf);
        }
    }
}

impl Decompress for HashedAccountBeforeTx {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        if value.len() < 32 {
            return Err(DecompressError::new(DatabaseError::Decode));
        }
        let hashed_address = B256::from_slice(&value[..32]);
        let info = if value.len() > 32 { Some(Account::decompress(&value[32..])?) } else { None };
        Ok(Self { hashed_address, info })
    }
}

/// Previous trie node value stored in V2 trie changesets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieChangeSetsEntry {
    /// Trie path. Acts as the dup-sort subkey.
    pub nibbles: StoredNibblesSubKey,
    /// Node value before the block, or `None` if the node did not exist.
    pub node: Option<BranchNodeCompact>,
}

impl ValueWithSubKey for TrieChangeSetsEntry {
    type SubKey = StoredNibblesSubKey;

    fn get_subkey(&self) -> Self::SubKey {
        self.nibbles.clone()
    }
}

impl Compress for TrieChangeSetsEntry {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = self.nibbles.to_compact(buf);
        if let Some(node) = &self.node {
            let _ = node.to_compact(buf);
        }
    }
}

impl Decompress for TrieChangeSetsEntry {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        if value.is_empty() {
            return Err(DecompressError::new(DatabaseError::Decode));
        }

        let (nibbles, rest) = StoredNibblesSubKey::from_compact(value, NIBBLE_SUBKEY_LEN);
        let node = if rest.is_empty() {
            None
        } else {
            Some(BranchNodeCompact::from_compact(rest, rest.len()).0)
        };
        Ok(Self { nibbles, node })
    }
}
