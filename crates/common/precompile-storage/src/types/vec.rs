//! Dynamic array (`Vec<T>`) implementation for the storage traits.
//!
//! # Storage Layout
//!
//! Vec uses Solidity-compatible dynamic array storage:
//! - **Base slot**: Stores the array length
//! - **Data slots**: Start at `keccak256(len_slot)`; elements packed where possible.

use std::ops::{Index, IndexMut};

use alloy_primitives::{Address, U256, keccak256};

use crate::{
    error::{BasePrecompileError, Result},
    packing::{PackedSlot, calc_element_loc, calc_packed_slot_count},
    provider::{Handler, Layout, LayoutCtx, Storable, StorableType, StorageOps},
    types::{HandlerCache, Slot},
};

impl<T> StorableType for Vec<T>
where
    T: Storable,
{
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler = VecHandler<T>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        VecHandler::new(slot, address)
    }
}

impl<T> Storable for Vec<T>
where
    T: Storable,
{
    fn load<S: StorageOps>(storage: &S, len_slot: U256, ctx: LayoutCtx) -> Result<Self> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");

        let length = load_checked_len(storage, len_slot)?;
        if length == 0 {
            return Ok(Self::new());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            load_packed_elements(storage, data_start, length, T::BYTES)
        } else {
            load_unpacked_elements(storage, data_start, length)
        }
    }

    fn store<S: StorageOps>(&self, storage: &mut S, len_slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");

        storage.store(len_slot, U256::from(self.len()))?;
        if self.is_empty() {
            return Ok(());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            store_packed_elements(self, storage, data_start, T::BYTES)
        } else {
            store_unpacked_elements(self, storage, data_start)
        }
    }

    fn delete<S: StorageOps>(storage: &mut S, len_slot: U256, ctx: LayoutCtx) -> Result<()> {
        debug_assert_eq!(ctx, LayoutCtx::FULL, "Dynamic arrays cannot be packed");

        let length = load_checked_len(storage, len_slot)?;
        storage.store(len_slot, U256::ZERO)?;

        if length == 0 {
            return Ok(());
        }

        let data_start = calc_data_slot(len_slot);
        if T::BYTES <= 16 {
            let slot_count = calc_packed_slot_count(length, T::BYTES);
            for slot_idx in 0..slot_count {
                storage.store(data_start + U256::from(slot_idx), U256::ZERO)?;
            }
        } else {
            for elem_idx in 0..length {
                let elem_slot = data_start + U256::from(elem_idx * T::SLOTS);
                T::delete(storage, elem_slot, LayoutCtx::FULL)?;
            }
        }

        Ok(())
    }
}

/// Type-safe handler for accessing `Vec<T>` in storage.
#[derive(Debug, Clone)]
pub struct VecHandler<T: Storable> {
    len_slot: U256,
    address: Address,
    cache: HandlerCache<usize, T::Handler>,
}

impl<T> Handler<Vec<T>> for VecHandler<T>
where
    T: Storable,
{
    #[inline]
    fn read(&self) -> Result<Vec<T>> {
        self.as_slot().read()
    }
    #[inline]
    fn write(&mut self, value: Vec<T>) -> Result<()> {
        self.as_slot().write(value)
    }
    #[inline]
    fn delete(&mut self) -> Result<()> {
        self.as_slot().delete()
    }
    #[inline]
    fn t_read(&self) -> Result<Vec<T>> {
        self.as_slot().t_read()
    }
    #[inline]
    fn t_write(&mut self, value: Vec<T>) -> Result<()> {
        self.as_slot().t_write(value)
    }
    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        self.as_slot().t_delete()
    }
}

impl<T> VecHandler<T>
where
    T: Storable,
{
    /// Creates a new handler for the vector at the given length slot and contract address.
    #[inline]
    pub fn new(len_slot: U256, address: Address) -> Self {
        Self { len_slot, address, cache: HandlerCache::new() }
    }

    const fn max_index() -> usize {
        if T::BYTES <= 16 { u32::MAX as usize / T::BYTES } else { u32::MAX as usize / T::SLOTS }
    }

    /// Returns the slot that stores the vector length.
    #[inline]
    pub const fn len_slot(&self) -> U256 {
        self.len_slot
    }

    /// Returns the slot where element data begins (`keccak256(len_slot)`).
    #[inline]
    pub fn data_slot(&self) -> U256 {
        calc_data_slot(self.len_slot)
    }

    #[inline]
    const fn as_slot(&self) -> Slot<Vec<T>> {
        Slot::new(self.len_slot, self.address)
    }

    /// Returns the number of elements in the vector.
    #[inline]
    pub fn len(&self) -> Result<usize> {
        let slot = Slot::<U256>::new(self.len_slot, self.address);
        load_checked_len(&slot, self.len_slot)
    }

    /// Returns whether the vector is empty.
    #[inline]
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    #[inline]
    fn compute_handler(data_start: U256, address: Address, index: usize) -> T::Handler {
        let (slot, layout_ctx) = if T::BYTES <= 16 {
            let location = calc_element_loc(index, T::BYTES);
            (
                data_start + U256::from(location.offset_slots),
                LayoutCtx::packed(location.offset_bytes),
            )
        } else {
            (data_start + U256::from(index * T::SLOTS), LayoutCtx::FULL)
        };
        T::handle(slot, layout_ctx, address)
    }

    /// Returns a handler for the element at the given index, or `None` if out of bounds.
    pub fn at(&self, index: usize) -> Result<Option<&T::Handler>> {
        if index >= self.len()? {
            return Ok(None);
        }
        let (data_start, address) = (self.data_slot(), self.address);
        Ok(Some(
            self.cache.get_or_insert(&index, || Self::compute_handler(data_start, address, index)),
        ))
    }

    /// Pushes a new element to the end of the vector.
    #[inline]
    pub fn push(&self, value: T) -> Result<()>
    where
        T: Storable,
        T::Handler: Handler<T>,
    {
        let length = self.len()?;
        if length >= Self::max_index() {
            return Err(BasePrecompileError::Fatal("Vec is at max capacity".into()));
        }
        let mut elem_slot = Self::compute_handler(self.data_slot(), self.address, length);
        elem_slot.write(value)?;
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address);
        length_slot.write(U256::from(length + 1))
    }

    /// Pops the last element from the vector. Returns `None` if empty.
    #[inline]
    pub fn pop(&self) -> Result<Option<T>>
    where
        T: Storable,
        T::Handler: Handler<T>,
    {
        let length = self.len()?;
        if length == 0 {
            return Ok(None);
        }
        let last_index = length - 1;
        let mut elem_slot = Self::compute_handler(self.data_slot(), self.address, last_index);
        let element = elem_slot.read()?;
        elem_slot.delete()?;
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address);
        length_slot.write(U256::from(last_index))?;
        Ok(Some(element))
    }
}

impl<T> Index<usize> for VecHandler<T>
where
    T: Storable,
{
    type Output = T::Handler;
    fn index(&self, index: usize) -> &Self::Output {
        let (data_start, address) = (self.data_slot(), self.address);
        self.cache.get_or_insert(&index, || Self::compute_handler(data_start, address, index))
    }
}

impl<T> IndexMut<usize> for VecHandler<T>
where
    T: Storable,
{
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        let (data_start, address) = (self.data_slot(), self.address);
        self.cache.get_or_insert_mut(&index, || Self::compute_handler(data_start, address, index))
    }
}

#[inline]
fn load_checked_len<S: StorageOps>(storage: &S, slot: U256) -> Result<usize> {
    let raw = storage.load(slot)?;
    if raw > U256::from(u32::MAX) {
        return Err(BasePrecompileError::under_overflow());
    }
    Ok(raw.to::<usize>())
}

#[inline]
pub(crate) fn calc_data_slot(len_slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0)
}

fn load_packed_elements<T, S>(
    storage: &S,
    data_start: U256,
    length: usize,
    byte_count: usize,
) -> Result<Vec<T>>
where
    T: Storable,
    S: StorageOps,
{
    let elements_per_slot = 32 / byte_count;
    let slot_count = calc_packed_slot_count(length, byte_count);
    let mut result = Vec::new();
    let mut current_offset = 0;

    for slot_idx in 0..slot_count {
        let slot_addr = data_start + U256::from(slot_idx);
        let slot_value = storage.load(slot_addr)?;
        let slot_packed = PackedSlot(slot_value);

        let elements_in_this_slot = if slot_idx == slot_count - 1 {
            length - (slot_idx * elements_per_slot)
        } else {
            elements_per_slot
        };

        for _ in 0..elements_in_this_slot {
            let elem = T::load(&slot_packed, slot_addr, LayoutCtx::packed(current_offset))?;
            result.push(elem);
            current_offset += byte_count;
            if current_offset >= 32 {
                current_offset = 0;
            }
        }

        current_offset = 0;
    }

    Ok(result)
}

fn store_packed_elements<T, S>(
    elements: &[T],
    storage: &mut S,
    data_start: U256,
    byte_count: usize,
) -> Result<()>
where
    T: Storable,
    S: StorageOps,
{
    let elements_per_slot = 32 / byte_count;
    let slot_count = calc_packed_slot_count(elements.len(), byte_count);

    for slot_idx in 0..slot_count {
        let slot_addr = data_start + U256::from(slot_idx);
        let start_elem = slot_idx * elements_per_slot;
        let end_elem = (start_elem + elements_per_slot).min(elements.len());
        let slot_value = build_packed_slot(&elements[start_elem..end_elem], byte_count)?;
        storage.store(slot_addr, slot_value)?;
    }

    Ok(())
}

fn build_packed_slot<T>(elements: &[T], byte_count: usize) -> Result<U256>
where
    T: Storable,
{
    let mut slot_value = PackedSlot(U256::ZERO);
    let mut current_offset = 0;
    for elem in elements {
        elem.store(&mut slot_value, U256::ZERO, LayoutCtx::packed(current_offset))?;
        current_offset += byte_count;
    }
    Ok(slot_value.0)
}

fn load_unpacked_elements<T, S>(storage: &S, data_start: U256, length: usize) -> Result<Vec<T>>
where
    T: Storable,
    S: StorageOps,
{
    let mut result = Vec::new();
    for index in 0..length {
        let elem_slot = data_start + U256::from(index * T::SLOTS);
        result.push(T::load(storage, elem_slot, LayoutCtx::FULL)?);
    }
    Ok(result)
}

fn store_unpacked_elements<T, S>(elements: &[T], storage: &mut S, data_start: U256) -> Result<()>
where
    T: Storable,
    S: StorageOps,
{
    for (idx, elem) in elements.iter().enumerate() {
        let elem_slot = data_start + U256::from(idx * T::SLOTS);
        elem.store(storage, elem_slot, LayoutCtx::FULL)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hashmap::setup_storage, packing::gen_word_from, storage_ctx::StorageCtx};

    #[test]
    fn test_vec_empty_roundtrip() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let len_slot = U256::from(100u64);
            let mut slot = Slot::<Vec<u8>>::new(len_slot, address);
            slot.write(vec![]).unwrap();
            let loaded: Vec<u8> = slot.read().unwrap();
            assert!(loaded.is_empty());
        });
    }

    #[test]
    fn test_vec_u8_roundtrip() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let len_slot = U256::from(200u64);
            let data = vec![10u8, 20, 30, 40, 50];
            let mut slot = Slot::<Vec<u8>>::new(len_slot, address);
            slot.write(data.clone()).unwrap();
            assert_eq!(slot.read().unwrap(), data);
            slot.delete().unwrap();
            let loaded: Vec<u8> = slot.read().unwrap();
            assert!(loaded.is_empty());
        });
    }

    #[test]
    fn test_vec_u8_explicit_slot_packing() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let len_slot = U256::from(2000u64);
            let data = vec![10u8, 20, 30, 40, 50];
            VecHandler::<u8>::new(len_slot, address).write(data).unwrap();

            let length = U256::handle(len_slot, LayoutCtx::FULL, address).read().unwrap();
            assert_eq!(length, U256::from(5u64));

            let data_start = calc_data_slot(len_slot);
            let slot_data = U256::handle(data_start, LayoutCtx::FULL, address).read().unwrap();
            let expected = gen_word_from(&["0x32", "0x28", "0x1e", "0x14", "0x0a"]);
            assert_eq!(slot_data, expected, "u8 packing should match Solidity layout");
        });
    }

    #[test]
    fn test_vec_data_slot_derivation() {
        let len_slot = U256::from(42u64);
        let data_slot = calc_data_slot(len_slot);
        let expected = U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0);
        assert_eq!(data_slot, expected);
    }

    #[test]
    fn test_vec_handler_push_pop() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let len_slot = U256::from(300u64);
            let handler = VecHandler::<U256>::new(len_slot, address);

            let vals: Vec<U256> = (0..5).map(U256::from).collect();
            for &v in &vals {
                handler.push(v).unwrap();
            }
            assert_eq!(handler.len().unwrap(), 5);

            for &v in vals.iter().rev() {
                assert_eq!(handler.pop().unwrap(), Some(v));
            }
            assert_eq!(handler.len().unwrap(), 0);
            assert_eq!(handler.pop().unwrap(), None);
        });
    }

    #[test]
    fn test_vec_length_overflow() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, || {
            let mut len_slot = Slot::<U256>::new(U256::ZERO, address);
            let handler = VecHandler::<u32>::new(U256::ZERO, address);

            len_slot.write(U256::from(0x0004000000000000u64)).unwrap();
            assert_eq!(handler.len(), Err(BasePrecompileError::under_overflow()));

            len_slot.write(U256::from(u32::MAX)).unwrap();
            assert_eq!(handler.len().unwrap(), u32::MAX as usize);

            len_slot.write(U256::from(u32::MAX as u64 + 1)).unwrap();
            assert_eq!(handler.len(), Err(BasePrecompileError::under_overflow()));
        });
    }
}
