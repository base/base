//! Type-safe wrapper for a single EVM storage slot.

use core::marker::PhantomData;

use alloy_primitives::{Address, U256};

use crate::{
    error::{BasePrecompileError, Result},
    packing::FieldLocation,
    provider::{Handler, LayoutCtx, Storable, StorableType, StorageOps},
    storage_ctx::StorageCtx,
};

/// Type-safe wrapper for a single EVM storage slot.
#[derive(Debug, Clone)]
pub struct Slot<'a, T> {
    slot: U256,
    ctx: LayoutCtx,
    address: Address,
    storage: StorageCtx<'a>,
    _ty: PhantomData<T>,
}

impl<'a, T> Slot<'a, T> {
    /// Creates a full-slot accessor at the given slot number and contract address.
    #[inline]
    pub const fn new(slot: U256, address: Address, storage: StorageCtx<'a>) -> Self {
        Self { slot, ctx: LayoutCtx::FULL, address, storage, _ty: PhantomData }
    }

    /// Creates a slot with an explicit [`LayoutCtx`] (for packed fields).
    #[inline]
    pub const fn new_with_ctx(
        slot: U256,
        ctx: LayoutCtx,
        address: Address,
        storage: StorageCtx<'a>,
    ) -> Self {
        Self { slot, ctx, address, storage, _ty: PhantomData }
    }

    /// Creates a full-slot accessor at `base_slot + offset_slots`.
    #[inline]
    pub fn new_at_offset(
        base_slot: U256,
        offset_slots: usize,
        address: Address,
        storage: StorageCtx<'a>,
    ) -> Result<Self> {
        Ok(Self {
            slot: base_slot
                .checked_add(U256::from_limbs([offset_slots as u64, 0, 0, 0]))
                .ok_or(BasePrecompileError::SlotOverflow)?,
            ctx: LayoutCtx::FULL,
            address,
            storage,
            _ty: PhantomData,
        })
    }

    /// Creates a packed-field accessor using a [`FieldLocation`] from `#[derive(Storable)]`.
    #[inline]
    pub fn new_at_loc(
        base_slot: U256,
        loc: FieldLocation,
        address: Address,
        storage: StorageCtx<'a>,
    ) -> Result<Self>
    where
        T: StorableType,
    {
        debug_assert!(T::IS_PACKABLE, "`fn new_at_loc` can only be used with packable types");
        Ok(Self {
            slot: base_slot
                .checked_add(U256::from_limbs([loc.offset_slots as u64, 0, 0, 0]))
                .ok_or(BasePrecompileError::SlotOverflow)?,
            ctx: LayoutCtx::packed(loc.offset_bytes),
            address,
            storage,
            _ty: PhantomData,
        })
    }

    /// Returns the storage slot number.
    #[inline]
    pub const fn slot(&self) -> U256 {
        self.slot
    }

    /// Returns the byte offset within the slot (for packed fields), or `None` for full-slot.
    #[inline]
    pub const fn offset(&self) -> Option<usize> {
        self.ctx.packed_offset()
    }
}

impl<T> StorageOps for Slot<'_, T> {
    fn load(&self, slot: U256) -> Result<U256> {
        self.storage.sload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        self.storage.sstore(self.address, slot, value)
    }
}

struct TransientOps<'a> {
    address: Address,
    storage: StorageCtx<'a>,
}

impl StorageOps for TransientOps<'_> {
    fn load(&self, slot: U256) -> Result<U256> {
        self.storage.tload(self.address, slot)
    }

    fn store(&mut self, slot: U256, value: U256) -> Result<()> {
        self.storage.tstore(self.address, slot, value)
    }
}

impl<'a, T: Storable> Slot<'a, T> {
    const fn transient(&self) -> TransientOps<'a> {
        TransientOps { address: self.address, storage: self.storage }
    }
}

impl<T: Storable> Handler<T> for Slot<'_, T> {
    #[inline]
    fn read(&self) -> Result<T> {
        T::load(self, self.slot, self.ctx)
    }

    #[inline]
    fn write(&mut self, value: T) -> Result<()> {
        value.store(self, self.slot, self.ctx)
    }

    #[inline]
    fn delete(&mut self) -> Result<()> {
        T::delete(self, self.slot, self.ctx)
    }

    #[inline]
    fn t_read(&self) -> Result<T> {
        T::load(&self.transient(), self.slot, self.ctx)
    }

    #[inline]
    fn t_write(&mut self, value: T) -> Result<()> {
        value.store(&mut self.transient(), self.slot, self.ctx)
    }

    #[inline]
    fn t_delete(&mut self) -> Result<()> {
        T::delete(&mut self.transient(), self.slot, self.ctx)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use proptest::prelude::*;

    use super::*;
    use crate::{hashmap::setup_storage, provider::StorageKey};

    fn arb_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    #[test]
    fn test_slot_size() {
        assert_eq!(size_of::<Slot<'_, U256>>(), 72);
        assert_eq!(size_of::<Slot<'_, Address>>(), 72);
        assert_eq!(size_of::<Slot<'_, bool>>(), 72);
    }

    #[test]
    fn test_slot_read_write_types() -> crate::error::Result<()> {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let mut u256_slot = Slot::<U256>::new(U256::ZERO, address, ctx);
            let val = U256::from(42u64);
            u256_slot.write(val)?;
            assert_eq!(u256_slot.read()?, val);

            let mut addr_slot = Slot::<Address>::new(U256::ONE, address, ctx);
            let test_addr = Address::from([0xab; 20]);
            addr_slot.write(test_addr)?;
            assert_eq!(addr_slot.read()?, test_addr);

            let mut bool_slot = Slot::<bool>::new(U256::from(2), address, ctx);
            bool_slot.write(true)?;
            assert!(bool_slot.read()?);

            Ok(())
        })
    }

    #[test]
    fn test_transient_persistence_isolation() -> crate::error::Result<()> {
        let (mut storage, address) = setup_storage();
        let slot_num = U256::from(7u64);
        let t_value = U256::from(100u64);
        let s_value = U256::from(200u64);

        StorageCtx::enter(&mut storage, |ctx| -> crate::error::Result<()> {
            let mut slot = Slot::<U256>::new(slot_num, address, ctx);
            slot.write(s_value)?;
            slot.t_write(t_value)?;
            assert_eq!(slot.read()?, s_value);
            assert_eq!(slot.t_read()?, t_value);
            Ok(())
        })?;

        storage.clear_transient();

        StorageCtx::enter(&mut storage, |ctx| {
            let slot = Slot::<U256>::new(slot_num, address, ctx);
            assert_eq!(slot.read()?, s_value);
            assert_eq!(slot.t_read()?, U256::ZERO);
            Ok(())
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn proptest_slot_isolation(
            s1 in arb_u256(), s2 in arb_u256(),
            v1 in arb_u256(), v2 in arb_u256()
        ) {
            let (mut storage, address) = setup_storage();
            StorageCtx::enter(&mut storage, |ctx| -> std::result::Result<(), TestCaseError> {
                let mut slot1 = Slot::<U256>::new(s1, address, ctx);
                let mut slot2 = Slot::<U256>::new(s2, address, ctx);
                slot1.write(v1).unwrap();
                slot2.write(v2).unwrap();
                prop_assert_eq!(slot1.read().unwrap(), v1);
                prop_assert_eq!(slot2.read().unwrap(), v2);
                Ok(())
            })?;
        }
    }

    #[test]
    fn test_slot_at_offset() -> crate::error::Result<()> {
        let (mut storage, address) = setup_storage();
        StorageCtx::enter(&mut storage, |ctx| {
            let pair_key = B256::random();
            let base = pair_key.mapping_slot(U256::ZERO);
            let test_addr = Address::from([0x22; 20]);

            let mut slot = Slot::<Address>::new_at_offset(base, 0, address, ctx)?;
            slot.write(test_addr)?;
            assert_eq!(slot.read()?, test_addr);
            slot.delete()?;
            assert_eq!(slot.read()?, Address::ZERO);
            Ok(())
        })
    }
}
