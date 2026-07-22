use std::collections::BTreeMap;

use alloy_consensus::EMPTY_ROOT_HASH;
use alloy_primitives::keccak256;
use alloy_rlp::EMPTY_STRING_CODE;
use base_proof_mpt::ordered_trie_with_encoder;
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use sha2::Digest;

use crate::{Result, WitnessError};

/// A validated, deduplicating collection of witness preimages.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreimageMap {
    preimages: BTreeMap<PreimageKey, Vec<u8>>,
}

impl PreimageMap {
    /// Creates an empty preimage map.
    pub const fn new() -> Self {
        Self { preimages: BTreeMap::new() }
    }

    /// Returns the number of unique preimages.
    pub fn len(&self) -> usize {
        self.preimages.len()
    }

    /// Returns whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.preimages.is_empty()
    }

    /// Inserts one preimage after validating hash-based keys.
    pub fn insert(&mut self, key: PreimageKey, value: Vec<u8>) -> Result<()> {
        let digest = match key.key_type() {
            PreimageKeyType::Keccak256 => Some(keccak256(&value).0),
            PreimageKeyType::Sha256 => Some(sha2::Sha256::digest(&value).into()),
            PreimageKeyType::Local
            | PreimageKeyType::GlobalGeneric
            | PreimageKeyType::Blob
            | PreimageKeyType::Precompile => None,
        };
        if digest.is_some_and(|digest| PreimageKey::new(digest, key.key_type()) != key) {
            return Err(WitnessError::InvalidPreimage(key));
        }

        if let Some(existing) = self.preimages.get(&key) {
            if existing != &value {
                return Err(WitnessError::ConflictingPreimage(key));
            }
            return Ok(());
        }

        self.preimages.insert(key, value);
        Ok(())
    }

    /// Inserts a Keccak-256-addressed value.
    pub fn insert_keccak(&mut self, value: Vec<u8>) -> Result<()> {
        let key = PreimageKey::new_keccak256(keccak256(&value).0);
        self.insert(key, value)
    }

    /// Merges another validated map.
    pub fn extend(&mut self, other: Self) -> Result<()> {
        for (key, value) in other.preimages {
            self.insert(key, value)?;
        }
        Ok(())
    }

    /// Inserts every proof node for an ordered Merkle Patricia Trie.
    pub fn insert_ordered_trie<T: AsRef<[u8]>>(&mut self, values: &[T]) -> Result<()> {
        if values.is_empty() {
            return self.insert(
                PreimageKey::new(*EMPTY_ROOT_HASH, PreimageKeyType::Keccak256),
                vec![EMPTY_STRING_CODE],
            );
        }

        let mut hash_builder = ordered_trie_with_encoder(values, |node, buf| {
            buf.put_slice(node.as_ref());
        });
        hash_builder.root();
        for (_, value) in hash_builder.take_proof_nodes().into_inner() {
            self.insert_keccak(value.into())?;
        }
        Ok(())
    }

    /// Consumes the map into the enclave's existing preimage vector format.
    pub fn into_preimages(self) -> Vec<(PreimageKey, Vec<u8>)> {
        self.preimages.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    #[test]
    fn validates_and_deduplicates_preimages() {
        let value = b"witness".to_vec();
        let key = PreimageKey::new_keccak256(keccak256(&value).0);
        let mut map = PreimageMap::new();

        map.insert(key, value.clone()).unwrap();
        map.insert(key, value).unwrap();

        assert_eq!(map.len(), 1);
        assert!(matches!(
            map.insert(key, b"different".to_vec()),
            Err(WitnessError::InvalidPreimage(_))
        ));

        let local_key = PreimageKey::new_local(1);
        map.insert(local_key, b"first".to_vec()).unwrap();
        assert!(matches!(
            map.insert(local_key, b"second".to_vec()),
            Err(WitnessError::ConflictingPreimage(_))
        ));
    }

    #[test]
    fn inserts_empty_ordered_trie() {
        let mut map = PreimageMap::new();
        map.insert_ordered_trie::<Vec<u8>>(&[]).unwrap();
        assert_eq!(map.len(), 1);
    }
}
