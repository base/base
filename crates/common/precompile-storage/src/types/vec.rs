//! Dynamic array (`Vec<T>`) implementation for the storage traits.
//!
//! # Storage Layout
//!
//! Vec uses Solidity-compatible dynamic array storage:
//! - **Base slot**: Stores the array length
//! - **Data slots**: Start at `keccak256(len_slot)`; elements packed where possible.

use alloc::vec::Vec;

use alloy_primitives::{Address, U256, keccak256};

use crate::{
    error::{BasePrecompileError, Result},
    packing::{PackedSlot, calc_element_loc, calc_packed_slot_count, create_element_mask},
    provider::{Handler, Layout, LayoutCtx, Storable, StorableType, StorageOps},
    types::{HandlerCache, Slot},
};

impl<T> StorableType for Vec<T>
where
    T: Storable,
{
    const LAYOUT: Layout = Layout::Slots(1);
    const IS_DYNAMIC: bool = true;
    type Handler<'a> = VecHandler<'a, T>;

    fn handle<'a>(
        slot: U256,
        _ctx: LayoutCtx,
        address: Address,
        storage: crate::StorageCtx<'a>,
    ) -> Self::Handler<'a> {
        VecHandler::new(slot, address, storage)
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
                storage.store(
                    data_start
                        .checked_add(U256::from(slot_idx))
                        .ok_or(BasePrecompileError::SlotOverflow)?,
                    U256::ZERO,
                )?;
            }
        } else {
            for elem_idx in 0..length {
                let elem_slot = data_start
                    .checked_add(U256::from(elem_idx * T::SLOTS))
                    .ok_or(BasePrecompileError::SlotOverflow)?;
                T::delete(storage, elem_slot, LayoutCtx::FULL)?;
            }
        }

        Ok(())
    }
}

/// Type-safe handler for accessing `Vec<T>` in storage.
#[derive(Debug, Clone)]
pub struct VecHandler<'a, T: Storable> {
    len_slot: U256,
    address: Address,
    storage: crate::StorageCtx<'a>,
    cache: HandlerCache<usize, T::Handler<'a>>,
}

impl<T> Handler<Vec<T>> for VecHandler<'_, T>
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

impl<'a, T> VecHandler<'a, T>
where
    T: Storable,
{
    /// Creates a new handler for the vector at the given length slot and contract address.
    #[inline]
    pub const fn new(len_slot: U256, address: Address, storage: crate::StorageCtx<'a>) -> Self {
        Self { len_slot, address, storage, cache: HandlerCache::new() }
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
    const fn as_slot(&self) -> Slot<'a, Vec<T>> {
        Slot::new(self.len_slot, self.address, self.storage)
    }

    /// Returns the number of elements in the vector.
    #[inline]
    pub fn len(&self) -> Result<usize> {
        let slot = Slot::<U256>::new(self.len_slot, self.address, self.storage);
        load_checked_len(&slot, self.len_slot)
    }

    /// Returns whether the vector is empty.
    #[inline]
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    #[inline]
    fn compute_handler(
        data_start: U256,
        address: Address,
        storage: crate::StorageCtx<'a>,
        index: usize,
    ) -> T::Handler<'a> {
        let (slot, layout_ctx) = if T::BYTES <= 16 {
            let location = calc_element_loc(index, T::BYTES);
            (
                data_start.checked_add(U256::from(location.offset_slots)).expect("slot overflow"),
                LayoutCtx::packed(location.offset_bytes),
            )
        } else {
            (
                data_start.checked_add(U256::from(index * T::SLOTS)).expect("slot overflow"),
                LayoutCtx::FULL,
            )
        };
        T::handle(slot, layout_ctx, address, storage)
    }

    /// Returns a handler for the element at the given index, or `None` if out of bounds.
    pub fn at(&self, index: usize) -> Result<Option<&T::Handler<'a>>> {
        if index >= self.len()? {
            return Ok(None);
        }
        let (data_start, address, storage) = (self.data_slot(), self.address, self.storage);
        Ok(Some(
            self.cache.get_or_insert(&index, || {
                Self::compute_handler(data_start, address, storage, index)
            }),
        ))
    }

    /// Pushes a new element to the end of the vector.
    #[inline]
    pub fn push(&self, value: T) -> Result<()>
    where
        T: Storable,
        T::Handler<'a>: Handler<T>,
    {
        let length = self.len()?;
        if length >= Self::max_index() {
            return Err(BasePrecompileError::Fatal("Vec is at max capacity".into()));
        }
        let mut elem_slot =
            Self::compute_handler(self.data_slot(), self.address, self.storage, length);
        elem_slot.write(value)?;
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address, self.storage);
        length_slot.write(U256::from(length + 1))
    }

    /// Pops the last element from the vector. Returns `None` if empty.
    #[inline]
    pub fn pop(&self) -> Result<Option<T>>
    where
        T: Storable,
        T::Handler<'a>: Handler<T>,
    {
        let length = self.len()?;
        if length == 0 {
            return Ok(None);
        }
        let last_index = length - 1;
        let mut elem_slot =
            Self::compute_handler(self.data_slot(), self.address, self.storage, last_index);
        let element = elem_slot.read()?;
        elem_slot.delete()?;
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address, self.storage);
        length_slot.write(U256::from(last_index))?;
        Ok(Some(element))
    }

    /// Shortens the vector to `new_len` elements, clearing all vacated storage slots.
    ///
    /// If `new_len` is greater than or equal to the current length, this has no effect.
    /// Each element slot is explicitly zeroed before the length counter is decremented,
    /// preventing stale tail data from being exposed by a future length-slot corruption.
    ///
    /// For packed element types (`T::BYTES <= 16`), elements in the "boundary" slot (the
    /// last slot that contains both kept and removed elements) are cleared individually
    /// using read-modify-write. Slots that are fully removed are zeroed in a single store.
    pub fn truncate(&self, new_len: usize) -> Result<()>
    where
        T: Storable,
        T::Handler<'a>: Handler<T>,
    {
        let old_len = self.len()?;
        if new_len >= old_len {
            return Ok(());
        }
        let data_start = self.data_slot();
        if T::BYTES <= 16 {
            let elems_per_slot = 32 / T::BYTES;
            // `first_full_tail_slot` is the index of the first slot that contains only
            // elements being removed (no kept elements). Slots before it may contain a
            // mix of kept and removed elements and require element-by-element clearing.
            let first_full_tail_slot = new_len.div_ceil(elems_per_slot);

            // Clear elements in the partial boundary slot (if any) with a single
            // SLOAD + mask + SSTORE. These elements share the slot with kept elements,
            // so we compute one combined clear-mask for all removed positions and apply
            // it in one read-modify-write rather than one per element.
            let boundary_slot_end = (first_full_tail_slot * elems_per_slot).min(old_len);
            if boundary_slot_end > new_len {
                let boundary_slot_addr = data_start + U256::from(new_len / elems_per_slot);
                let current = self.storage.sload(self.address, boundary_slot_addr)?;
                let mut combined_clear_mask = U256::ZERO;
                for index in new_len..boundary_slot_end {
                    let byte_offset = (index % elems_per_slot) * T::BYTES;
                    combined_clear_mask |= create_element_mask(T::BYTES) << (byte_offset * 8);
                }
                self.storage.sstore(
                    self.address,
                    boundary_slot_addr,
                    current & !combined_clear_mask,
                )?;
            }

            // Zero all fully-removed tail slots in a single store each.
            let last_slot = calc_packed_slot_count(old_len, T::BYTES);
            for slot_idx in first_full_tail_slot..last_slot {
                let slot_addr = data_start + U256::from(slot_idx);
                self.storage.sstore(self.address, slot_addr, U256::ZERO)?;
            }
        } else {
            // Unpacked types: each element occupies one or more full slots;
            // delegate to the element handler's delete to zero every occupied slot.
            for index in new_len..old_len {
                let mut elem = Self::compute_handler(data_start, self.address, self.storage, index);
                elem.delete()?;
            }
        }
        // Update the length counter only after all vacated slots are cleared.
        let mut length_slot = Slot::<U256>::new(self.len_slot, self.address, self.storage);
        length_slot.write(U256::from(new_len))
    }

    /// Clears the vector, removing all elements and zeroing all data slots.
    ///
    /// Equivalent to `truncate(0)`. Matches Solidity's `delete arr` semantics.
    pub fn clear(&self) -> Result<()>
    where
        T: Storable,
        T::Handler<'a>: Handler<T>,
    {
        self.truncate(0)
    }
}

impl<'a, T> VecHandler<'a, T>
where
    T: Storable,
{
    /// Returns a handler for the element at `index`, bounds-checked against a caller-supplied
    /// `len` (no `sload` — pure integer comparison).
    ///
    /// Callers must supply a `len` freshly read from `self.len()` and not invalidated by a
    /// subsequent `push`/`pop`. Returns `Err(Fatal)` if `index >= len`.
    #[inline]
    pub(crate) fn at_with_len(&self, index: usize, len: usize) -> Result<&T::Handler<'a>> {
        if index >= len {
            return Err(BasePrecompileError::Fatal(
                "vec index out of bounds: position invariant violated".into(),
            ));
        }
        let (data_start, address, storage) = (self.data_slot(), self.address, self.storage);
        Ok(self
            .cache
            .get_or_insert(&index, || Self::compute_handler(data_start, address, storage, index)))
    }

    /// Mutable variant of [`at_with_len`].
    #[inline]
    pub(crate) fn at_mut_with_len(
        &mut self,
        index: usize,
        len: usize,
    ) -> Result<&mut T::Handler<'a>> {
        if index >= len {
            return Err(BasePrecompileError::Fatal(
                "vec index out of bounds: position invariant violated".into(),
            ));
        }
        let (data_start, address, storage) = (self.data_slot(), self.address, self.storage);
        Ok(self.cache.get_or_insert_mut(&index, || {
            Self::compute_handler(data_start, address, storage, index)
        }))
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
        let slot_addr = data_start
            .checked_add(U256::from(slot_idx))
            .ok_or(BasePrecompileError::SlotOverflow)?;
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
        let slot_addr = data_start
            .checked_add(U256::from(slot_idx))
            .ok_or(BasePrecompileError::SlotOverflow)?;
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
        let elem_slot = data_start
            .checked_add(U256::from(index * T::SLOTS))
            .ok_or(BasePrecompileError::SlotOverflow)?;
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
        let elem_slot = data_start
            .checked_add(U256::from(idx * T::SLOTS))
            .ok_or(BasePrecompileError::SlotOverflow)?;
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
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(100u64);
            let mut slot = Slot::<Vec<u8>>::new(len_slot, address, ctx);
            slot.write(vec![]).unwrap();
            let loaded: Vec<u8> = slot.read().unwrap();
            assert!(loaded.is_empty());
        });
    }

    #[test]
    fn test_vec_u8_roundtrip() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(200u64);
            let data = vec![10u8, 20, 30, 40, 50];
            let mut slot = Slot::<Vec<u8>>::new(len_slot, address, ctx);
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
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(2000u64);
            let data = vec![10u8, 20, 30, 40, 50];
            VecHandler::<u8>::new(len_slot, address, ctx).write(data).unwrap();

            let length = U256::handle(len_slot, LayoutCtx::FULL, address, ctx).read().unwrap();
            assert_eq!(length, U256::from(5u64));

            let data_start = calc_data_slot(len_slot);
            let slot_data = U256::handle(data_start, LayoutCtx::FULL, address, ctx).read().unwrap();
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
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(300u64);
            let handler = VecHandler::<U256>::new(len_slot, address, ctx);

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
    fn test_vec_at_oob_returns_none() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(900u64);
            let handler = VecHandler::<U256>::new(len_slot, address, ctx);
            assert!(handler.at(0).unwrap().is_none());
        });
    }

    #[test]
    fn test_vec_at_with_len_oob_returns_err() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(901u64);
            let handler = VecHandler::<U256>::new(len_slot, address, ctx);
            assert!(handler.at_with_len(0, 0).is_err());
            assert!(handler.at_with_len(5, 5).is_err());
            assert!(handler.at_with_len(0, 1).is_ok());
        });
    }

    #[test]
    fn test_vec_length_overflow() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let mut len_slot = Slot::<U256>::new(U256::ZERO, address, ctx);
            let handler = VecHandler::<u32>::new(U256::ZERO, address, ctx);

            len_slot.write(U256::from(0x0004000000000000u64)).unwrap();
            assert_eq!(handler.len(), Err(BasePrecompileError::under_overflow()));

            len_slot.write(U256::from(u32::MAX)).unwrap();
            assert_eq!(handler.len().unwrap(), u32::MAX as usize);

            len_slot.write(U256::from(u32::MAX as u64 + 1)).unwrap();
            assert_eq!(handler.len(), Err(BasePrecompileError::under_overflow()));
        });
    }

    #[test]
    fn test_vec_truncate_unpacked() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(400u64);
            let handler = VecHandler::<U256>::new(len_slot, address, ctx);

            let vals: Vec<U256> = (0..5).map(U256::from).collect();
            for &v in &vals {
                handler.push(v).unwrap();
            }
            assert_eq!(handler.len().unwrap(), 5);

            // truncate to 3 — elements 3 and 4 should be cleared.
            handler.truncate(3).unwrap();
            assert_eq!(handler.len().unwrap(), 3);

            let data_start = calc_data_slot(len_slot);
            let slot3 = U256::handle(data_start + U256::from(3), LayoutCtx::FULL, address, ctx)
                .read()
                .unwrap();
            let slot4 = U256::handle(data_start + U256::from(4), LayoutCtx::FULL, address, ctx)
                .read()
                .unwrap();
            assert_eq!(slot3, U256::ZERO, "vacated slot 3 must be cleared");
            assert_eq!(slot4, U256::ZERO, "vacated slot 4 must be cleared");

            // remaining elements are intact.
            for i in 0..3 {
                assert_eq!(handler.at(i).unwrap().unwrap().read().unwrap(), U256::from(i));
            }

            // truncate past current length is a no-op.
            handler.truncate(10).unwrap();
            assert_eq!(handler.len().unwrap(), 3);
        });
    }

    #[test]
    fn test_vec_truncate_packed_boundary_slot() {
        // Vec<u8>: 32 elements per slot.  Push 35 elements; truncate to 33.
        // Slot 0 (indices 0-31): fully kept.
        // Slot 1 (indices 32-34): index 32 kept, indices 33-34 cleared individually.
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(500u64);
            let handler = VecHandler::<u8>::new(len_slot, address, ctx);

            for i in 0u8..35 {
                handler.push(i).unwrap();
            }
            assert_eq!(handler.len().unwrap(), 35);

            handler.truncate(33).unwrap();
            assert_eq!(handler.len().unwrap(), 33);

            // Slot 1 must still contain element 32 (offset 0) and have bytes 1-2 cleared.
            let data_start = calc_data_slot(len_slot);
            let slot1 = U256::handle(data_start + U256::from(1), LayoutCtx::FULL, address, ctx)
                .read()
                .unwrap();
            // Only byte 0 (element 32 = 0x20) should survive.
            let expected = gen_word_from(&["0x20"]);
            assert_eq!(slot1, expected, "boundary slot must retain element 32 only");
        });
    }

    #[test]
    fn test_vec_truncate_packed_full_tail_slots() {
        // Vec<u8>: 32 elements per slot.  Push 65 elements; truncate to 32.
        // Slot 0 (indices 0-31): fully kept.
        // Slot 1 (indices 32-63): fully removed — zeroed in one store.
        // Slot 2 (index 64): fully removed — zeroed in one store.
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(600u64);
            let handler = VecHandler::<u8>::new(len_slot, address, ctx);

            for i in 0u8..65 {
                handler.push(i).unwrap();
            }
            assert_eq!(handler.len().unwrap(), 65);

            handler.truncate(32).unwrap();
            assert_eq!(handler.len().unwrap(), 32);

            let data_start = calc_data_slot(len_slot);
            let slot1 = U256::handle(data_start + U256::from(1), LayoutCtx::FULL, address, ctx)
                .read()
                .unwrap();
            let slot2 = U256::handle(data_start + U256::from(2), LayoutCtx::FULL, address, ctx)
                .read()
                .unwrap();
            assert_eq!(slot1, U256::ZERO, "fully-removed slot 1 must be zeroed");
            assert_eq!(slot2, U256::ZERO, "fully-removed slot 2 must be zeroed");

            // slot 0 must be fully intact.
            let slot0 = U256::handle(data_start, LayoutCtx::FULL, address, ctx).read().unwrap();
            let expected = gen_word_from(&[
                "0x1f", "0x1e", "0x1d", "0x1c", "0x1b", "0x1a", "0x19", "0x18", "0x17", "0x16",
                "0x15", "0x14", "0x13", "0x12", "0x11", "0x10", "0x0f", "0x0e", "0x0d", "0x0c",
                "0x0b", "0x0a", "0x09", "0x08", "0x07", "0x06", "0x05", "0x04", "0x03", "0x02",
                "0x01", "0x00",
            ]);
            assert_eq!(slot0, expected, "slot 0 must be fully intact after truncate");
        });
    }

    #[test]
    fn test_vec_clear() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let len_slot = U256::from(700u64);
            let handler = VecHandler::<U256>::new(len_slot, address, ctx);

            for i in 0u64..4 {
                handler.push(U256::from(i)).unwrap();
            }
            assert_eq!(handler.len().unwrap(), 4);

            handler.clear().unwrap();
            assert_eq!(handler.len().unwrap(), 0);
            assert!(handler.is_empty().unwrap());

            let data_start = calc_data_slot(len_slot);
            for i in 0u64..4 {
                let slot_val =
                    U256::handle(data_start + U256::from(i), LayoutCtx::FULL, address, ctx)
                        .read()
                        .unwrap();
                assert_eq!(slot_val, U256::ZERO, "slot {i} must be cleared after clear()");
            }

            // push-after-clear must work correctly.
            handler.push(U256::from(42u64)).unwrap();
            assert_eq!(handler.len().unwrap(), 1);
            assert_eq!(handler.at(0).unwrap().unwrap().read().unwrap(), U256::from(42u64));
        });
    }
}
