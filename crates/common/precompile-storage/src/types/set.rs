//! [`OpenZeppelin`](https://github.com/OpenZeppelin/openzeppelin-contracts) `EnumerableSet` implementation for EVM storage using Rust primitives.
//! <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/master/contracts/utils/structs/EnumerableSet.sol>
//!
//! # Storage Layout
//!
//! - **Values Vec**: A `Vec<T>` storing all set elements at `keccak256(base_slot)`
//! - **Positions Mapping**: A `Mapping<T, u32>` at `base_slot + 1` (1-indexed, 0 = not present)

use alloc::{
    collections::BTreeSet,
    vec::{IntoIter, Vec},
};
use core::{fmt, ops::Deref, slice};

use alloy_primitives::{Address, U256};

use crate::{
    error::{BasePrecompileError, Result},
    provider::{Handler, Layout, LayoutCtx, Storable, StorableType, StorageKey, StorageOps},
    types::{MappingHandler, Slot, vec::VecHandler},
};

/// Read-only snapshot of a set stored via [`SetHandler`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Set<T>(Vec<T>);

impl<T> Set<T> {
    /// Creates a new empty set.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Creates a set from a vector already known to contain no duplicates.
    pub const fn new_unchecked(vec: Vec<T>) -> Self {
        Self(vec)
    }
}

impl<T> Deref for Set<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> From<Set<T>> for Vec<T> {
    fn from(set: Set<T>) -> Self {
        set.0
    }
}

impl<T: Eq + Clone + Ord> From<Vec<T>> for Set<T> {
    fn from(vec: Vec<T>) -> Self {
        let mut seen = BTreeSet::new();
        let mut deduped = Vec::new();
        for item in vec {
            if seen.insert(item.clone()) {
                deduped.push(item);
            }
        }
        Self(deduped)
    }
}

impl<T: Eq + Clone + Ord> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<T> IntoIterator for Set<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Set<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Type-safe handler for accessing `Set<T>` in storage.
pub struct SetHandler<'a, T>
where
    T: Storable + StorageKey + Eq + Clone + Ord,
{
    values: VecHandler<'a, T>,
    positions: MappingHandler<'a, T, u32>,
    base_slot: U256,
    address: Address,
    storage: crate::StorageCtx<'a>,
}

/// Set occupies 2 slots: slot 0 = Vec length, slot 1 = positions mapping base.
impl<T> StorableType for Set<T>
where
    T: Storable + StorageKey + Eq + Clone + Ord,
{
    const LAYOUT: Layout = Layout::Slots(2);
    const IS_DYNAMIC: bool = true;
    type Handler<'a> = SetHandler<'a, T>;

    fn handle<'a>(
        slot: U256,
        _ctx: LayoutCtx,
        address: Address,
        storage: crate::StorageCtx<'a>,
    ) -> Self::Handler<'a> {
        SetHandler::new(slot, address, storage)
    }
}

impl<T> Storable for Set<T>
where
    T: Storable + StorageKey + Eq + Clone + Ord,
    for<'a> T::Handler<'a>: Handler<T>,
{
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let values: Vec<T> = Vec::load(storage, slot, LayoutCtx::FULL)?;
        Ok(Self(values))
    }

    fn store<S: StorageOps>(&self, _storage: &mut S, _slot: U256, _ctx: LayoutCtx) -> Result<()> {
        Err(BasePrecompileError::Fatal(
            "Set must be stored via SetHandler::write() to maintain position invariants".into(),
        ))
    }

    fn delete<S: StorageOps>(storage: &mut S, slot: U256, ctx: LayoutCtx) -> Result<()> {
        let values: Vec<T> = Vec::load(storage, slot, LayoutCtx::FULL)?;
        for value in values {
            let pos_slot = value.mapping_slot(slot + U256::ONE);
            <U256 as Storable>::delete(storage, pos_slot, LayoutCtx::FULL)?;
        }
        <Vec<T> as Storable>::delete(storage, slot, ctx)
    }
}

#[inline]
fn checked_position(index: usize) -> Result<u32> {
    u32::try_from(index)
        .ok()
        .and_then(|i| i.checked_add(1))
        .ok_or_else(BasePrecompileError::under_overflow)
}

impl<'a, T> SetHandler<'a, T>
where
    T: Storable + StorageKey + Eq + Clone + Ord,
{
    /// Creates a new handler for the set at the given base slot.
    pub fn new(base_slot: U256, address: Address, storage: crate::StorageCtx<'a>) -> Self {
        Self {
            values: VecHandler::new(base_slot, address, storage),
            positions: MappingHandler::new(base_slot + U256::ONE, address, storage),
            base_slot,
            address,
            storage,
        }
    }

    /// Returns the base storage slot for this set.
    pub const fn base_slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> Result<usize> {
        self.values.len()
    }

    /// Returns whether the set is empty.
    pub fn is_empty(&self) -> Result<bool> {
        self.values.is_empty()
    }

    /// Returns true if the value is in the set.
    pub fn contains(&self, value: &T) -> Result<bool>
    where
        T: StorageKey + Eq + Clone + Ord,
    {
        self.positions.at(value).read().map(|pos| pos != 0)
    }

    /// Inserts a value into the set. Returns `true` if newly inserted, `false` if already present.
    pub fn insert(&mut self, value: T) -> Result<bool>
    where
        T: StorageKey + Eq + Clone + Ord,
        T::Handler<'a>: Handler<T>,
    {
        if self.contains(&value)? {
            return Ok(false);
        }
        let length = self.values.len()?;
        self.positions.at_mut(&value).write(checked_position(length)?)?;
        self.values.push(value)?;
        Ok(true)
    }

    /// Removes a value from the set using swap-and-pop. Returns `true` if found and removed.
    pub fn remove(&mut self, value: &T) -> Result<bool>
    where
        T: StorageKey + Eq + Clone + Ord,
        T::Handler<'a>: Handler<T>,
    {
        let position = self.positions.at(value).read()?;
        if position == 0 {
            return Ok(false);
        }

        let len = self.values.len()?;
        let last_index = len - 1;
        let index = (position - 1) as usize;

        if index != last_index {
            let last_value = self.values.at_with_len(last_index, len)?.read()?;
            self.positions.at_mut(&last_value).write(position)?;
            self.values.at_mut_with_len(index, len)?.write(last_value)?;
        }

        self.values.at_mut_with_len(last_index, len)?.delete()?;
        Slot::<U256>::new(self.values.len_slot(), self.address, self.storage)
            .write(U256::from(last_index))?;
        self.positions.at_mut(value).delete()?;
        Ok(true)
    }

    /// Returns the value at the given index, or `None` if out of bounds.
    pub fn at(&self, index: usize) -> Result<Option<T>>
    where
        T::Handler<'a>: Handler<T>,
    {
        let len = self.len()?;
        if index >= len {
            return Ok(None);
        }
        Ok(Some(self.values.at_with_len(index, len)?.read()?))
    }

    /// Reads a contiguous range of elements from the set.
    pub fn read_range(&self, start: usize, end: usize) -> Result<Vec<T>>
    where
        T::Handler<'a>: Handler<T>,
    {
        let len = self.len()?;
        let end = end.min(len);
        let start = start.min(end);
        let mut result = Vec::new();
        for i in start..end {
            result.push(self.values.at_with_len(i, len)?.read()?);
        }
        Ok(result)
    }
}

impl<'a, T> Handler<Set<T>> for SetHandler<'a, T>
where
    T: Storable + StorageKey + Eq + Clone + Ord,
    for<'ctx> T::Handler<'ctx>: Handler<T>,
{
    fn read(&self) -> Result<Set<T>> {
        let len = self.len()?;
        let mut vec = Vec::new();
        for i in 0..len {
            vec.push(self.values.at_with_len(i, len)?.read()?);
        }
        Ok(Set(vec))
    }

    fn write(&mut self, value: Set<T>) -> Result<()> {
        let old_len = self.values.len()?;
        let new_len = value.0.len();

        for i in 0..old_len {
            let old_value = self.values.at_with_len(i, old_len)?.read()?;
            self.positions.at_mut(&old_value).delete()?;
        }

        for (index, new_value) in value.0.into_iter().enumerate() {
            self.positions.at_mut(&new_value).write(checked_position(index)?)?;
            self.values.at_mut_with_len(index, new_len)?.write(new_value)?;
        }

        for i in new_len..old_len {
            self.values.at_mut_with_len(i, old_len)?.delete()?;
        }

        if new_len != old_len {
            Slot::<U256>::new(self.values.len_slot(), self.address, self.storage)
                .write(U256::from(new_len))?;
        }

        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        let len = self.len()?;
        for i in 0..len {
            let value = self.values.at_with_len(i, len)?.read()?;
            self.positions.at_mut(&value).delete()?;
        }
        self.values.delete()
    }

    fn t_read(&self) -> Result<Set<T>> {
        unimplemented!("Set does not support transient storage")
    }
    fn t_write(&mut self, _: Set<T>) -> Result<()> {
        unimplemented!("Set does not support transient storage")
    }
    fn t_delete(&mut self) -> Result<()> {
        unimplemented!("Set does not support transient storage")
    }
}

impl<T> fmt::Debug for SetHandler<'_, T>
where
    T: Storable + StorageKey + Eq + Clone + Ord + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetHandler").field("base_slot", &self.base_slot).finish()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use rstest::rstest;

    use super::*;
    use crate::{hashmap::setup_storage, storage_ctx::StorageCtx};

    #[test]
    fn test_set_insert_contains_remove() {
        let (mut storage, contract_addr) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(500u64);
            let mut handler = SetHandler::<Address>::new(base, contract_addr, ctx);

            let a = Address::from([0x11; 20]);
            let b = Address::from([0x22; 20]);

            assert!(!handler.contains(&a).unwrap());
            assert!(handler.insert(a).unwrap());
            assert!(!handler.insert(a).unwrap()); // duplicate
            assert!(handler.contains(&a).unwrap());
            assert_eq!(handler.len().unwrap(), 1);

            assert!(handler.insert(b).unwrap());
            assert_eq!(handler.len().unwrap(), 2);

            assert!(handler.remove(&a).unwrap());
            assert!(!handler.contains(&a).unwrap());
            assert_eq!(handler.len().unwrap(), 1);

            assert!(!handler.remove(&a).unwrap()); // already removed
        });
    }

    #[test]
    fn test_set_read_write() {
        let (mut storage, contract_addr) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(600u64);
            let mut handler = SetHandler::<Address>::new(base, contract_addr, ctx);

            let addrs: Vec<Address> = (0..5u8).map(|i| Address::from([i; 20])).collect();
            let set = Set::from(addrs.clone());
            handler.write(set).unwrap();

            let loaded = handler.read().unwrap();
            assert_eq!(loaded.len(), addrs.len());
            for addr in &addrs {
                assert!(handler.contains(addr).unwrap());
            }
        });
    }

    /// (`initial_size`, `final_size`) — covers grow, shrink, and equal-size rewrite.
    #[rstest]
    #[case(3, 7)] // grow
    #[case(7, 3)] // shrink
    #[case(4, 4)] // same size, different contents
    fn test_set_write_len_slot_updated(#[case] initial: u8, #[case] final_size: u8) {
        let (mut storage, contract_addr) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(700u64);
            let mut handler = SetHandler::<Address>::new(base, contract_addr, ctx);

            // Use disjoint ranges so first and second share no elements.
            let first: Vec<Address> = (0..initial).map(|i| Address::from([i; 20])).collect();
            handler.write(Set::from(first.clone())).unwrap();
            assert_eq!(handler.len().unwrap(), initial as usize);

            let second: Vec<Address> =
                (100..100 + final_size).map(|i| Address::from([i; 20])).collect();
            handler.write(Set::from(second.clone())).unwrap();

            assert_eq!(handler.len().unwrap(), final_size as usize);
            let loaded = handler.read().unwrap();
            assert_eq!(loaded.len(), final_size as usize);
            for addr in &second {
                assert!(handler.contains(addr).unwrap());
            }
            for addr in &first {
                assert!(!handler.contains(addr).unwrap());
            }
        });
    }
}
