//! Fixed-size array handler for the storage traits.
//!
//! Fixed-size arrays `[T; N]` use Solidity-compatible array storage:
//! - **Base slot**: Arrays start directly at `base_slot` (not at keccak256)
//! - Small elements (`T::BYTES` ≤ 16) are packed; larger elements use full slots.

use core::ops::{Index, IndexMut};

use alloy_primitives::{Address, U256};

use crate::{
    error::Result,
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
    pub fn at(&mut self, index: usize) -> Option<&T::Handler<'a>> {
        if index >= N {
            return None;
        }
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        Some(
            self.cache.get_or_insert(&index, || {
                Self::compute_handler(base_slot, address, storage, index)
            }),
        )
    }

    #[inline]
    fn compute_handler(
        base_slot: U256,
        address: Address,
        storage: crate::StorageCtx<'a>,
        index: usize,
    ) -> T::Handler<'a> {
        let (slot, layout_ctx) = if T::BYTES <= 16 {
            let location = packing::calc_element_loc(index, T::BYTES);
            (
                base_slot + U256::from(location.offset_slots),
                LayoutCtx::packed(location.offset_bytes),
            )
        } else {
            (base_slot + U256::from(index * T::SLOTS), LayoutCtx::FULL)
        };
        T::handle(slot, layout_ctx, address, storage)
    }
}

impl<'a, T: StorableType, const N: usize> Index<usize> for ArrayHandler<'a, T, N> {
    type Output = T::Handler<'a>;

    fn index(&self, index: usize) -> &Self::Output {
        assert!(index < N, "index out of bounds: {index} >= {N}");
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache
            .get_or_insert(&index, || Self::compute_handler(base_slot, address, storage, index))
    }
}

impl<'a, T: StorableType, const N: usize> IndexMut<usize> for ArrayHandler<'a, T, N> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        assert!(index < N, "index out of bounds: {index} >= {N}");
        let (base_slot, address, storage) = (self.base_slot, self.address, self.storage);
        self.cache
            .get_or_insert_mut(&index, || Self::compute_handler(base_slot, address, storage, index))
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::*;
    use crate::{hashmap::setup_storage, provider::{Handler, LayoutCtx, StorableType}, storage_ctx::StorageCtx};

    // -- Packed arrays (T::BYTES <= 16, multiple elements per slot) ---------------

    #[test]
    fn test_packed_array_write_read_whole() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(10u64);
            let data: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
            let mut handler = ArrayHandler::<u32, 8>::new(base, address, ctx);
            handler.write(data).unwrap();
            let loaded = handler.read().unwrap();
            assert_eq!(loaded, data);
        });
    }

    #[test]
    fn test_packed_array_all_elements_survive_write() {
        // Write a full array and verify every element individually
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(20u64);
            let data: [u8; 32] = core::array::from_fn(|i| i as u8);
            let mut handler = ArrayHandler::<u8, 32>::new(base, address, ctx);
            handler.write(data).unwrap();
            let loaded = handler.read().unwrap();
            for i in 0..32usize {
                assert_eq!(loaded[i], i as u8, "mismatch at index {i}");
            }
        });
    }

    #[test]
    fn test_packed_array_overwrite_previous_value() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(30u64);
            let first: [u16; 4] = [10, 20, 30, 40];
            let second: [u16; 4] = [100, 200, 300, 400];
            let mut handler = ArrayHandler::<u16, 4>::new(base, address, ctx);
            handler.write(first).unwrap();
            handler.write(second).unwrap();
            assert_eq!(handler.read().unwrap(), second);
        });
    }

    #[test]
    fn test_packed_array_index_read_individual_elements() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(40u64);
            let data: [u32; 5] = [11, 22, 33, 44, 55];
            let mut handler = ArrayHandler::<u32, 5>::new(base, address, ctx);
            handler.write(data).unwrap();

            // Read each element via the index operator
            for (i, &expected) in data.iter().enumerate() {
                let got = handler[i].read().unwrap();
                assert_eq!(got, expected, "element {i} mismatch");
            }
        });
    }

    #[test]
    fn test_packed_array_out_of_bounds_at_returns_none() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(50u64);
            let mut handler = ArrayHandler::<u32, 4>::new(base, address, ctx);
            assert!(handler.at(4).is_none(), "index 4 should be out of bounds for N=4");
            assert!(handler.at(0).is_some());
        });
    }

    // -- Unpacked arrays (T::BYTES == 32, one element per slot) ------------------

    #[test]
    fn test_unpacked_array_write_read_whole() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(100u64);
            let data: [U256; 4] =
                [U256::from(1u64), U256::from(2u64), U256::from(3u64), U256::from(4u64)];
            let mut handler = ArrayHandler::<U256, 4>::new(base, address, ctx);
            handler.write(data).unwrap();
            assert_eq!(handler.read().unwrap(), data);
        });
    }

    #[test]
    fn test_unpacked_array_elements_stored_at_consecutive_raw_slots() {
        // Each U256 element must land at base_slot + i in raw storage,
        // not at some derived address. Read the slots directly via a
        // plain U256 handler to confirm layout without going through ArrayHandler.
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(200u64);
            let data: [U256; 3] = [U256::from(10u64), U256::from(20u64), U256::from(30u64)];
            let mut handler = ArrayHandler::<U256, 3>::new(base, address, ctx);
            handler.write(data).unwrap();

            // Read the underlying storage slots directly - bypasses ArrayHandler logic
            for (i, &expected) in data.iter().enumerate() {
                let raw_slot = U256::handle(base + U256::from(i), LayoutCtx::FULL, address, ctx);
                let raw_value = raw_slot.read().unwrap();
                assert_eq!(raw_value, expected, "raw slot {i} (slot {}) has wrong value", base + U256::from(i));
            }
        });
    }

    #[test]
    fn test_unpacked_array_overwrite_clears_previous() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(300u64);
            let first: [U256; 2] = [U256::from(0xDEADu64), U256::from(0xBEEFu64)];
            let second: [U256; 2] = [U256::from(1u64), U256::from(2u64)];
            let mut handler = ArrayHandler::<U256, 2>::new(base, address, ctx);
            handler.write(first).unwrap();
            handler.write(second).unwrap();
            assert_eq!(handler.read().unwrap(), second);
        });
    }

    // -- Delete roundtrips -------------------------------------------------------

    #[test]
    fn test_packed_array_delete_clears_storage() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(350u64);
            let data: [u32; 4] = [111, 222, 333, 444];
            let mut handler = ArrayHandler::<u32, 4>::new(base, address, ctx);
            handler.write(data).unwrap();
            handler.delete().unwrap();
            let loaded = handler.read().unwrap();
            assert_eq!(loaded, [0u32; 4], "delete should zero out all elements");
        });
    }

    #[test]
    fn test_unpacked_array_delete_clears_storage() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(360u64);
            let data: [U256; 3] = [U256::from(7u64), U256::from(8u64), U256::from(9u64)];
            let mut handler = ArrayHandler::<U256, 3>::new(base, address, ctx);
            handler.write(data).unwrap();
            handler.delete().unwrap();

            // Verify via raw slot reads that each slot was zeroed
            for i in 0..3usize {
                let raw_slot = U256::handle(base + U256::from(i), LayoutCtx::FULL, address, ctx);
                assert_eq!(raw_slot.read().unwrap(), U256::ZERO, "slot {i} should be zero after delete");
            }
        });
    }

    // -- Transient storage -------------------------------------------------------

    #[test]
    fn test_array_transient_write_read() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(700u64);
            let persistent_data: [U256; 2] = [U256::from(1u64), U256::from(2u64)];
            let transient_data: [U256; 2] = [U256::from(100u64), U256::from(200u64)];
            let mut handler = ArrayHandler::<U256, 2>::new(base, address, ctx);

            handler.write(persistent_data).unwrap();
            handler.t_write(transient_data).unwrap();

            // Transient and persistent reads must return their respective values
            assert_eq!(handler.read().unwrap(), persistent_data, "persistent read should be unaffected by t_write");
            assert_eq!(handler.t_read().unwrap(), transient_data, "t_read should return transient value");

            handler.t_delete().unwrap();
            assert_eq!(handler.t_read().unwrap(), [U256::ZERO; 2], "t_delete should clear transient storage");
            // Persistent storage must remain intact
            assert_eq!(handler.read().unwrap(), persistent_data, "persistent storage must survive t_delete");
        });
    }

    // -- Address arrays (T::BYTES == 20, packed) ---------------------------------

    #[test]
    fn test_address_array_roundtrip() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(400u64);
            let data: [Address; 3] = [
                Address::from([0x11; 20]),
                Address::from([0x22; 20]),
                Address::from([0x33; 20]),
            ];
            let mut handler = ArrayHandler::<Address, 3>::new(base, address, ctx);
            handler.write(data).unwrap();
            assert_eq!(handler.read().unwrap(), data);
        });
    }

    // -- Metadata helpers --------------------------------------------------------

    #[test]
    fn test_array_handler_metadata() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(500u64);
            let handler = ArrayHandler::<u32, 7>::new(base, address, ctx);
            assert_eq!(handler.len(), 7);
            assert!(!handler.is_empty());
            assert_eq!(handler.base_slot(), base);
        });
    }

    #[test]
    fn test_array_handler_empty_const() {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let base = U256::from(600u64);
            let mut handler = ArrayHandler::<u32, 0>::new(base, address, ctx);
            assert!(handler.is_empty());
            assert_eq!(handler.len(), 0);
            assert!(handler.at(0).is_none());
        });
    }
}
