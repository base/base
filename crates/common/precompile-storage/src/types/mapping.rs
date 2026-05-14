//! Type-safe wrapper for EVM storage mappings (hash-based key-value storage).

use std::{
    hash::Hash,
    ops::{Index, IndexMut},
};

use alloy_primitives::{Address, U256};

use crate::{
    provider::{Layout, LayoutCtx, StorableType, StorageKey},
    types::HandlerCache,
};

/// Type-safe access wrapper for EVM storage mappings.
#[derive(Debug, Clone)]
pub struct Mapping<K, V: StorableType> {
    base_slot: U256,
    address: Address,
    cache: HandlerCache<K, V::Handler>,
}

impl<K, V: StorableType> Mapping<K, V> {
    /// Creates a new mapping with the given base slot and contract address.
    #[inline]
    pub fn new(base_slot: U256, address: Address) -> Self {
        Self { base_slot, address, cache: HandlerCache::new() }
    }

    /// Returns the base storage slot for this mapping.
    #[inline]
    pub const fn slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns a handler for the given key (immutable access, cached).
    pub fn at(&self, key: &K) -> &V::Handler
    where
        K: StorageKey + Hash + Eq + Clone,
    {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache
            .get_or_insert(key, || V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address))
    }

    /// Returns a mutable handler for the given key (mutable access, cached).
    pub fn at_mut(&mut self, key: &K) -> &mut V::Handler
    where
        K: StorageKey + Hash + Eq + Clone,
    {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert_mut(key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V: StorableType> Default for Mapping<K, V> {
    fn default() -> Self {
        Self::new(U256::ZERO, Address::ZERO)
    }
}

impl<K, V: StorableType> Index<K> for Mapping<K, V>
where
    K: StorageKey + Hash + Eq + Clone,
{
    type Output = V::Handler;

    fn index(&self, key: K) -> &Self::Output {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V: StorableType> IndexMut<K> for Mapping<K, V>
where
    K: StorageKey + Hash + Eq + Clone,
{
    fn index_mut(&mut self, key: K) -> &mut Self::Output {
        let (base_slot, address) = (self.base_slot, self.address);
        self.cache.get_or_insert_mut(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address)
        })
    }
}

impl<K, V> StorableType for Mapping<K, V>
where
    V: StorableType,
{
    const LAYOUT: Layout = Layout::Slots(1);
    type Handler = Self;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Self::new(slot, address)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, keccak256};

    use super::*;

    fn old_mapping_slot<K: AsRef<[u8]>>(key: K, slot: U256) -> U256 {
        let key = key.as_ref();
        let mut buf = [0u8; 64];
        buf[32 - key.len()..32].copy_from_slice(key);
        buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    #[test]
    fn test_mapping_slot_encoding() {
        let key = Address::from([0x11; 20]);
        let base_slot = U256::from(42u64);

        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(key.as_ref());
        buf[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
        let expected = U256::from_be_bytes(keccak256(buf).0);
        let computed = key.mapping_slot(base_slot);

        assert_eq!(computed, expected);
    }

    #[test]
    fn test_mapping_slot_matches_old_impl() {
        let slot = U256::from(99u64);
        let addr = Address::from([0x33; 20]);
        assert_eq!(addr.mapping_slot(slot), old_mapping_slot(addr.as_slice(), slot));

        let b256 = B256::from([0x44; 32]);
        assert_eq!(b256.mapping_slot(slot), old_mapping_slot(b256.as_slice(), slot));
    }

    #[test]
    fn test_mapping_basic_properties() {
        let address = Address::from([0x10; 20]);
        let base_slot = U256::from(1u64);
        let mapping = Mapping::<Address, U256>::new(base_slot, address);

        let key = Address::from([0x20; 20]);
        let slot1 = &mapping[key];
        let slot2 = &mapping[key];
        assert_eq!(slot1.slot(), slot2.slot());

        let key1 = Address::from([0x21; 20]);
        let key2 = Address::from([0x22; 20]);
        assert_ne!(mapping[key1].slot(), mapping[key2].slot());
    }
}
