//! Transient-storage view of the in-flight EIP-8130 transaction context.

use alloy_primitives::{Address, B256, U256, address};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

/// Transient-storage-backed view of the in-flight EIP-8130 transaction context.
///
/// The resolved payer is written to transient storage at [`Self::ADDRESS`] by
/// the EIP-8130 execution layer at the start of transaction processing and
/// cleared automatically at transaction end. The precompile reads it without
/// exposing TLOAD opcode gas because EIP-8130 prices getter output separately.
///
/// For any non-EIP-8130 transaction (where nothing is written) [`Self::payer`]
/// falls back to `tx.origin`, so callers observe the actual transaction
/// originator uniformly across tx types.
#[derive(Debug)]
pub struct TxContextStorage<'a> {
    storage: StorageCtx<'a>,
}

impl<'a> TxContextStorage<'a> {
    /// Transaction context precompile address.
    ///
    /// Pinned to `TX_CONTEXT_ADDRESS` from the EIP-8130 constant table
    /// (`0x8130…aa02`, in the `0x8130…` / EIP-number namespace for EIP-8130
    /// system precompiles).
    pub const ADDRESS: Address = address!("813000000000000000000000000000000000aa02");

    /// Transient slot holding the resolved payer address.
    const PAYER_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);

    /// Creates a transaction context view over the active storage scope.
    pub const fn new(storage: StorageCtx<'a>) -> Self {
        Self { storage }
    }

    /// Returns the resolved payer, falling back to `tx.origin` when unset
    /// (i.e. outside an EIP-8130 transaction).
    pub fn payer(&self) -> Result<Address> {
        let raw = self.storage.tload_unmetered(Self::ADDRESS, Self::PAYER_SLOT)?;
        if raw.is_zero() {
            return Ok(self.storage.origin());
        }
        Ok(Address::from_word(B256::from(raw.to_be_bytes::<32>())))
    }

    /// Writes the resolved payer into transient storage.
    ///
    /// Intended for the EIP-8130 execution layer to call once at the start of
    /// transaction processing through the gas-free `JournalStorageProvider`.
    /// This publication write is covered by the protocol's intrinsic gas
    /// schedule and must not expose a TSTORE opcode charge. The value is
    /// cleared automatically when the transaction's transient storage is reset.
    ///
    /// # Errors
    /// Returns [`BasePrecompileError::assert_failed`] if `payer` is zero. A zero
    /// slot means "unset" and selects the `tx.origin` fallback, so writing zero
    /// would silently misattribute the transaction.
    pub fn set_payer(&mut self, payer: Address) -> Result<()> {
        if payer.is_zero() {
            return Err(BasePrecompileError::assert_failed());
        }
        self.storage.tstore(
            Self::ADDRESS,
            Self::PAYER_SLOT,
            U256::from_be_bytes(payer.into_word().0),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::tx_context::storage::TxContextStorage;

    const PAYER: Address = address!("0x2222222222222222222222222222222222222222");
    const ORIGIN: Address = address!("0x9999999999999999999999999999999999999999");

    #[test]
    fn payer_is_zero_when_origin_is_zero() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_eq!(TxContextStorage::new(ctx).payer().unwrap(), Address::ZERO);
        });
    }

    #[test]
    fn payer_falls_back_to_origin_when_unset() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_eq!(TxContextStorage::new(ctx).payer().unwrap(), ORIGIN);
        });
    }

    #[test]
    fn set_payer_overrides_origin_fallback() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut view = TxContextStorage::new(ctx);
            view.set_payer(PAYER).unwrap();
            assert_eq!(view.payer().unwrap(), PAYER);
        });
    }

    #[test]
    fn set_payer_rejects_zero() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            assert!(TxContextStorage::new(ctx).set_payer(Address::ZERO).is_err());
        });
    }

    #[test]
    fn payer_clears_to_origin_with_transient_storage() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).set_payer(PAYER).unwrap();
            ctx.clear_transient();
            assert_eq!(TxContextStorage::new(ctx).payer().unwrap(), ORIGIN);
        });
    }
}
