use core::{
    hash::{Hash, Hasher},
    mem,
};

use alloy_primitives::{
    Address, B256, FixedBytes,
    map::{FbBuildHasher, HashMap, HashSet},
};

use crate::interpreter::Word;

/// Hash map keyed by storage account and slot.
pub type StorageKeyMap<V> = HashMap<StorageKey, V, FbBuildHasher<52>>;

/// Hash set keyed by storage account and slot.
pub type StorageKeySet = HashSet<StorageKey, FbBuildHasher<52>>;

/// Storage key for account-address and storage-slot pairs.
#[derive(Clone, Copy, Debug, Default, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(C)]
pub struct StorageKey {
    address: Address,
    key: B256,
}

const _: () = assert!(mem::size_of::<StorageKey>() == mem::size_of::<FixedBytes<52>>());

impl StorageKey {
    /// Creates a storage key from an account address and slot.
    #[inline]
    pub fn new(address: Address, key: Word) -> Self {
        Self { address, key: B256::from(key) }
    }

    /// Returns the account address.
    #[inline]
    pub const fn address(self) -> Address {
        self.address
    }

    /// Returns the storage slot.
    #[inline]
    pub const fn key(self) -> Word {
        Word::from_be_bytes(self.key.0)
    }

    #[inline]
    const fn as_fixed_bytes(&self) -> &FixedBytes<52> {
        // SAFETY: `StorageKey` is `repr(C)` and contains exactly an `Address` followed by a `B256`;
        // both are transparent fixed-byte wrappers, and the size assertion above guarantees 52
        // bytes.
        unsafe { &*(core::ptr::from_ref(self).cast::<FixedBytes<52>>()) }
    }
}

impl PartialEq for StorageKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_fixed_bytes() == other.as_fixed_bytes()
    }
}

impl Hash for StorageKey {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.as_fixed_bytes().as_slice());
    }
}
