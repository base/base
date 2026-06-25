//! Fixed-size array handler for the storage traits.
//!
//! Fixed-size arrays `[T; N]` use Solidity-compatible array storage:
//! - **Base slot**: Arrays start directly at `base_slot` (not at keccak256)
//! - Small elements (`T::BYTES` ≤ 16) are packed; larger elements use full slots.

use alloy_primitives::{Address, U256};

use crate::{
    error::{BasePrecompileError, Result},
    packing,
    provider::{Handler, LayoutCtx, Storable, StorableType},
    types::{HandlerCache, Slot},
};

// fixed-size arrays: [T; N] for primitive types T and sizes 1-32
base_precompile_macros::storable_arrays!();
// nested arrays: [[T; M]; N] for small primitive types
base_precompile_macros::storable_nested_arrays!();

/// Type-safe handler for accessing fixed-size arrays `[T; N]` in storage.
#[derive(Debug, Clone)]
pub struct ArrayHandler<'a, T: StorableType, const N: usize> {
    base_slot: U256,
    address: Address,
    storage: crate::StorageCtx<'a>,
    cache: HandlerCache<usize, T::Handler<'a>>,
}

impl<'a, T: StorableType, const N: usize> ArrayHandler<'a, T, N> {
    /// Creates a new handler for the array at the given base slot and address.
    #[inline]
    pub const fn new(base_slot: U256, address: Address, storage: crate::StorageCtx<'a>) -> Self {
        Self { base_slot, address, storage, cache: HandlerCache::new() }
    }

    #[inline]
    const fn as_slot(&self) -> Slot<'a, [T; N]> {
        Slot::new(self.base_slot, self.address, self.storage)
    }

    /// Returns the base storage slot where this array's data is stored.
    #[inline]
    pub const fn base_slot(&self) -> U256 {
        self.base_slot
    }

    /// Returns the array size (compile-time constant `N`).
    #[inline]
    pub const fn len(&self) -> usize {
        N
    }

    /// Returns whether the array is empty (`N == 0`).
    #[inline]
    pub const fn is_empty(&self) -> bool {
        N == 0
    }

    /// Returns a handler for the element at the given index, or `None` if out of bounds.
    #[inline]
    pub fn at(&self, index: usize) -> Result<Option<&T::Handler<'a>>> {
        if index >= N {
            return Ok(None);
        }
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        Ok(Some(self.cache.get_or_try_insert(&index, || {
            Self::try_compute_handler(base_slot, address, storage, index)
        })?))
    }

    /// Returns a mutable handler for the element at the given index, or `None` if out of bounds.
    #[inline]
    pub fn at_mut(&mut self, index: usize) -> Result<Option<&mut T::Handler<'a>>> {
        if index >= N {
            return Ok(None);
        }
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        Ok(Some(self.cache.get_or_try_insert_mut(&index, || {
            Self::try_compute_handler(base_slot, address, storage, index)
        })?))
    }

    #[inline]
    fn try_compute_handler(
        base_slot: U256,
        address: Address,
        storage: crate::StorageCtx<'a>,
        index: usize,
    ) -> Result<T::Handler<'a>> {
        let (slot, layout_ctx) = if T::BYTES <= 16 {
            let location = packing::calc_element_loc(index, T::BYTES);
            (
                base_slot
                    .checked_add(U256::from(location.offset_slots))
                    .ok_or(BasePrecompileError::SlotOverflow)?,
                LayoutCtx::packed(location.offset_bytes),
            )
        } else {
            (
                base_slot
                    .checked_add(U256::from(index * T::SLOTS))
                    .ok_or(BasePrecompileError::SlotOverflow)?,
                LayoutCtx::FULL,
            )
        };
        Ok(T::handle(slot, layout_ctx, address, storage))
    }
}

impl<T: StorableType, const N: usize> Handler<[T; N]> for ArrayHandler<'_, T, N>
where
    [T; N]: Storable,
{
    #[inline]
    fn read(&self) -> Result<[T; N]> {
        self.as_slot().read()
    }

    #[inline]
    fn write(&mut self, value: [T; N]) -> Result<()> {
        self.as_slot().write(value)
    }

    #[inline]
    fn delete(&mut self) -> Result<()> {
        self.as_slot().delete()
    }

    #[inline]
    fn t_read(&self) -> Result<[T; N]> {
        self.as_slot().t_read()
    }

    #[inline]
    fn t_write(&mut self, value: [T; N]) -> Result<()> {
        self.as_slot().t_write(value)
    }

    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        self.as_slot().t_delete()
    }
}
