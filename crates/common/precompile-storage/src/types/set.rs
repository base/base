//! [`OpenZeppelin`](https://github.com/OpenZeppelin/openzeppelin-contracts) `EnumerableSet` implementation for EVM storage using Rust primitives.
//! <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/master/contracts/utils/structs/EnumerableSet.sol>
//!
//! # Storage Layout
//!
//! - **Values Vec**: A `Vec<T>` storing all set elements at `keccak256(base_slot)`
//! - **Positions Mapping**: A `Mapping<T, u32>` at `base_slot + 1` (1-indexed, 0 = not present)

<<<<<<< HEAD
use alloy_primitives::{Address, U256};
use std::{
    collections::HashSet,
    fmt,
    hash::Hash,
    ops::Deref,
};
=======
use std::{collections::HashSet, fmt, hash::Hash, ops::Deref};

use alloy_primitives::{Address, U256};
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

use crate::{
    error::{BasePrecompileError, Result},
    provider::{Handler, Layout, LayoutCtx, Storable, StorableType, StorageKey, StorageOps},
    types::{Mapping, Slot, vec::VecHandler},
};

/// Read-only snapshot of a set stored via [`SetHandler`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Set<T>(Vec<T>);

impl<T> Set<T> {
    /// Creates a new empty set.
<<<<<<< HEAD
    pub const fn new() -> Self { Self(Vec::new()) }

    /// Creates a set from a vector already known to contain no duplicates.
    pub const fn new_unchecked(vec: Vec<T>) -> Self { Self(vec) }
=======
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Creates a set from a vector already known to contain no duplicates.
    pub const fn new_unchecked(vec: Vec<T>) -> Self {
        Self(vec)
    }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

impl<T> Deref for Set<T> {
    type Target = [T];
<<<<<<< HEAD
    fn deref(&self) -> &[T] { &self.0 }
}

impl<T> From<Set<T>> for Vec<T> {
    fn from(set: Set<T>) -> Self { set.0 }
=======
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> From<Set<T>> for Vec<T> {
    fn from(set: Set<T>) -> Self {
        set.0
    }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

impl<T: Eq + Hash + Clone> From<Vec<T>> for Set<T> {
    fn from(vec: Vec<T>) -> Self {
        let (mut seen, mut deduped) = (HashSet::new(), Vec::new());
        for item in vec {
<<<<<<< HEAD
            if seen.insert(item.clone()) { deduped.push(item); }
=======
            if seen.insert(item.clone()) {
                deduped.push(item);
            }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        }
        Self(deduped)
    }
}

impl<T: Eq + Hash + Clone> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl<T> IntoIterator for Set<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
<<<<<<< HEAD
    fn into_iter(self) -> Self::IntoIter { self.0.into_iter() }
=======
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

impl<'a, T> IntoIterator for &'a Set<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
<<<<<<< HEAD
    fn into_iter(self) -> Self::IntoIter { self.0.iter() }
=======
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

/// Type-safe handler for accessing `Set<T>` in storage.
pub struct SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    values: VecHandler<T>,
    positions: Mapping<T, u32>,
    base_slot: U256,
    address: Address,
}

/// Set occupies 2 slots: slot 0 = Vec length, slot 1 = positions mapping base.
impl<T> StorableType for Set<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    const LAYOUT: Layout = Layout::Slots(2);
    const IS_DYNAMIC: bool = true;
    type Handler = SetHandler<T>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        SetHandler::new(slot, address)
    }
}

impl<T> Storable for Set<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
    T::Handler: Handler<T>,
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

impl<T> SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
{
    /// Creates a new handler for the set at the given base slot.
    pub fn new(base_slot: U256, address: Address) -> Self {
        Self {
            values: VecHandler::new(base_slot, address),
            positions: Mapping::new(base_slot + U256::ONE, address),
            base_slot,
            address,
        }
    }

    /// Returns the base storage slot for this set.
<<<<<<< HEAD
    pub const fn base_slot(&self) -> U256 { self.base_slot }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> Result<usize> { self.values.len() }

    /// Returns whether the set is empty.
    pub fn is_empty(&self) -> Result<bool> { self.values.is_empty() }
=======
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
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

    /// Returns true if the value is in the set.
    pub fn contains(&self, value: &T) -> Result<bool>
    where
        T: StorageKey + Hash + Eq + Clone,
    {
        self.positions.at(value).read().map(|pos| pos != 0)
    }

    /// Inserts a value into the set. Returns `true` if newly inserted, `false` if already present.
    pub fn insert(&mut self, value: T) -> Result<bool>
    where
        T: StorageKey + Hash + Eq + Clone,
        T::Handler: Handler<T>,
    {
<<<<<<< HEAD
        if self.contains(&value)? { return Ok(false); }
=======
        if self.contains(&value)? {
            return Ok(false);
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        let length = self.values.len()?;
        self.positions.at_mut(&value).write(checked_position(length)?)?;
        self.values.push(value)?;
        Ok(true)
    }

    /// Removes a value from the set using swap-and-pop. Returns `true` if found and removed.
    pub fn remove(&mut self, value: &T) -> Result<bool>
    where
        T: StorageKey + Hash + Eq + Clone,
        T::Handler: Handler<T>,
    {
        let position = self.positions.at(value).read()?;
<<<<<<< HEAD
        if position == 0 { return Ok(false); }
=======
        if position == 0 {
            return Ok(false);
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

        let len = self.values.len()?;
        let last_index = len - 1;
        let index = (position - 1) as usize;

        if index != last_index {
            let last_value = self.values[last_index].read()?;
            self.positions.at_mut(&last_value).write(position)?;
            self.values[index].write(last_value)?;
        }

        self.values[last_index].delete()?;
        Slot::<U256>::new(self.values.len_slot(), self.address).write(U256::from(last_index))?;
        self.positions.at_mut(value).delete()?;
        Ok(true)
    }

    /// Returns the value at the given index, or `None` if out of bounds.
    pub fn at(&self, index: usize) -> Result<Option<T>>
    where
        T::Handler: Handler<T>,
    {
<<<<<<< HEAD
        if index >= self.len()? { return Ok(None); }
=======
        if index >= self.len()? {
            return Ok(None);
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        Ok(Some(self.values[index].read()?))
    }

    /// Reads a contiguous range of elements from the set.
    pub fn read_range(&self, start: usize, end: usize) -> Result<Vec<T>>
    where
        T::Handler: Handler<T>,
    {
        let len = self.len()?;
        let end = end.min(len);
        let start = start.min(end);
        let mut result = Vec::new();
<<<<<<< HEAD
        for i in start..end { result.push(self.values[i].read()?); }
=======
        for i in start..end {
            result.push(self.values[i].read()?);
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        Ok(result)
    }
}

impl<T> Handler<Set<T>> for SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone,
    T::Handler: Handler<T>,
{
    fn read(&self) -> Result<Set<T>> {
        let len = self.len()?;
        let mut vec = Vec::new();
<<<<<<< HEAD
        for i in 0..len { vec.push(self.values[i].read()?); }
=======
        for i in 0..len {
            vec.push(self.values[i].read()?);
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        Ok(Set(vec))
    }

    fn write(&mut self, value: Set<T>) -> Result<()> {
        let old_len = self.values.len()?;
        let new_len = value.0.len();

        for i in 0..old_len {
            let old_value = self.values[i].read()?;
            self.positions.at_mut(&old_value).delete()?;
        }

        for (index, new_value) in value.0.into_iter().enumerate() {
            self.positions.at_mut(&new_value).write(checked_position(index)?)?;
            self.values[index].write(new_value)?;
        }

        Slot::<U256>::new(self.values.len_slot(), self.address).write(U256::from(new_len))?;

<<<<<<< HEAD
        for i in new_len..old_len { self.values[i].delete()?; }
=======
        for i in new_len..old_len {
            self.values[i].delete()?;
        }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
        Ok(())
    }

    fn delete(&mut self) -> Result<()> {
        let len = self.len()?;
        for i in 0..len {
            let value = self.values[i].read()?;
            self.positions.at_mut(&value).delete()?;
        }
        self.values.delete()
    }

<<<<<<< HEAD
    fn t_read(&self) -> Result<Set<T>> { unimplemented!("Set does not support transient storage") }
    fn t_write(&mut self, _: Set<T>) -> Result<()> { unimplemented!("Set does not support transient storage") }
    fn t_delete(&mut self) -> Result<()> { unimplemented!("Set does not support transient storage") }
=======
    fn t_read(&self) -> Result<Set<T>> {
        unimplemented!("Set does not support transient storage")
    }
    fn t_write(&mut self, _: Set<T>) -> Result<()> {
        unimplemented!("Set does not support transient storage")
    }
    fn t_delete(&mut self) -> Result<()> {
        unimplemented!("Set does not support transient storage")
    }
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
}

impl<T> fmt::Debug for SetHandler<T>
where
    T: Storable + StorageKey + Hash + Eq + Clone + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
<<<<<<< HEAD
        f.debug_struct("SetHandler")
            .field("base_slot", &self.base_slot)
            .finish()
=======
        f.debug_struct("SetHandler").field("base_slot", &self.base_slot).finish()
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5
    }
}

#[cfg(test)]
mod tests {
<<<<<<< HEAD
    use super::*;
    use crate::{hashmap::setup_storage, storage_ctx::StorageCtx};
    use alloy_primitives::Address;
=======
    use alloy_primitives::Address;

    use super::*;
    use crate::{hashmap::setup_storage, storage_ctx::StorageCtx};
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

    #[test]
    fn test_set_insert_contains_remove() {
        let (mut storage, contract_addr) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let base = U256::from(500u64);
            let mut handler = SetHandler::<Address>::new(base, contract_addr);

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
        StorageCtx::enter(&mut storage, || {
            let base = U256::from(600u64);
            let mut handler = SetHandler::<Address>::new(base, contract_addr);

            let addrs: Vec<Address> = (0..5u8).map(|i| Address::from([i; 20])).collect();
            let set = Set::from(addrs.clone());
<<<<<<< HEAD
            handler.write(set.clone()).unwrap();
=======
            handler.write(set).unwrap();
>>>>>>> d46df9d45d1cea16baddff35c768ecceea2e02d5

            let loaded = handler.read().unwrap();
            assert_eq!(loaded.len(), addrs.len());
            for addr in &addrs {
                assert!(handler.contains(addr).unwrap());
            }
        });
    }
}
