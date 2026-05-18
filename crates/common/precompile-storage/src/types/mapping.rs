//! Type-safe wrapper for EVM storage mappings (hash-based key-value storage).

use core::{
    marker::PhantomData,
    ops::{Index, IndexMut},
};

use alloy_primitives::{Address, U256};

use crate::{
    provider::{Layout, LayoutCtx, StorableType, StorageKey},
    types::HandlerCache,
};

/// Marker type for EVM storage mappings.
#[derive(Debug, Clone)]
pub struct Mapping<K, V: StorableType> {
    _key: PhantomData<K>,
    _value: PhantomData<V>,
}

/// Type-safe access wrapper for EVM storage mappings.
#[derive(Debug, Clone)]
pub struct MappingHandler<'a, K, V: StorableType> {
    base_slot: U256,
    address: Address,
    storage: crate::StorageCtx<'a>,
    cache: HandlerCache<K, V::Handler<'a>>,
}

impl<K, V: StorableType> Default for Mapping<K, V> {
    fn default() -> Self {
        Self { _key: PhantomData, _value: PhantomData }
    }
}

impl<'a, K, V: StorableType> MappingHandler<'a, K, V> {
    /// Creates a new mapping with the given base slot and contract address.
    #[inline]
    pub const fn new(base_slot: U256, address: Address, storage: crate::StorageCtx<'a>) -> Self {
        Self { base_slot, address, storage, cache: HandlerCache::new() }
    }

    /// Returns the base storage slot for this mapping.
    #[inline]
    pub const fn slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns a handler for the given key (immutable access, cached).
    pub fn at(&self, key: &K) -> &V::Handler<'a>
    where
        K: StorageKey + Eq + Clone + Ord,
    {
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache.get_or_insert(key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address, storage)
        })
    }

    /// Returns a mutable handler for the given key (mutable access, cached).
    pub fn at_mut(&mut self, key: &K) -> &mut V::Handler<'a>
    where
        K: StorageKey + Eq + Clone + Ord,
    {
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache.get_or_insert_mut(key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address, storage)
        })
    }
}

impl<'a, K, V: StorableType> Index<K> for MappingHandler<'a, K, V>
where
    K: StorageKey + Eq + Clone + Ord,
{
    type Output = V::Handler<'a>;

    fn index(&self, key: K) -> &Self::Output {
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache.get_or_insert(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address, storage)
        })
    }
}

impl<'a, K, V: StorableType> IndexMut<K> for MappingHandler<'a, K, V>
where
    K: StorageKey + Eq + Clone + Ord,
{
    fn index_mut(&mut self, key: K) -> &mut Self::Output {
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache.get_or_insert_mut(&key, || {
            V::handle(key.mapping_slot(base_slot), LayoutCtx::FULL, address, storage)
        })
    }
}

impl<K, V> StorableType for Mapping<K, V>
where
    V: StorableType,
{
    const LAYOUT: Layout = Layout::Slots(1);
    type Handler<'a> = MappingHandler<'a, K, V>;

    fn handle<'a>(
        slot: U256,
        _ctx: LayoutCtx,
        address: Address,
        storage: crate::StorageCtx<'a>,
    ) -> Self::Handler<'a> {
        MappingHandler::new(slot, address, storage)
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
        let (mut storage, _) = crate::hashmap::setup_storage();
        crate::StorageCtx::enter(&mut storage, |ctx| {
            let mapping = MappingHandler::<Address, U256>::new(base_slot, address, ctx);

            let key = Address::from([0x20; 20]);
            let slot1 = &mapping[key];
            let slot2 = &mapping[key];
            assert_eq!(slot1.slot(), slot2.slot());

            let key1 = Address::from([0x21; 20]);
            let key2 = Address::from([0x22; 20]);
            assert_ne!(mapping[key1].slot(), mapping[key2].slot());
        });
    }
}
