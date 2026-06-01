//! Storage layout and constants for the EIP-8130 transaction context precompile.

use alloy_primitives::{Address, B256, U256, address};
use base_precompile_storage::{Result, StorageCtx};

/// Transient-storage-backed view of the in-flight EIP-8130 transaction context.
///
/// The resolved sender, payer, and sender owner id are written to transient
/// storage at [`Self::ADDRESS`] by the EIP-8130 execution layer at the start of
/// transaction processing and cleared automatically at transaction end. The
/// precompile only reads them back, so for any non-EIP-8130 transaction (where
/// nothing is written) every getter returns the zero value.
#[derive(Debug)]
pub struct TxContextStorage<'a> {
    storage: StorageCtx<'a>,
}

impl<'a> TxContextStorage<'a> {
    /// Transaction context precompile address.
    ///
    /// Provisional: the EIP-8130 `TX_CONTEXT_ADDRESS` is not finalized upstream.
    /// This follows the Base singleton convention (`0x8453…`) and can be
    /// renumbered when the spec pins a concrete value.
    pub const ADDRESS: Address = address!("8453000000000000000000000000000000000003");

    /// Transient slot holding the resolved sender address.
    const SENDER_SLOT: U256 = U256::ZERO;
    /// Transient slot holding the resolved payer address.
    const PAYER_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
    /// Transient slot holding the sender owner id.
    const SENDER_OWNER_ID_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

    /// Creates a transaction context view over the active storage scope.
    pub const fn new(storage: StorageCtx<'a>) -> Self {
        Self { storage }
    }

    /// Returns the resolved sender, or [`Address::ZERO`] when unset.
    pub fn sender(&self) -> Result<Address> {
        let raw = self.storage.tload(Self::ADDRESS, Self::SENDER_SLOT)?;
        Ok(Address::from_word(B256::from(raw.to_be_bytes::<32>())))
    }

    /// Returns the resolved payer, or [`Address::ZERO`] when unset.
    pub fn payer(&self) -> Result<Address> {
        let raw = self.storage.tload(Self::ADDRESS, Self::PAYER_SLOT)?;
        Ok(Address::from_word(B256::from(raw.to_be_bytes::<32>())))
    }

    /// Returns the sender owner id, or [`B256::ZERO`] when unset.
    pub fn sender_owner_id(&self) -> Result<B256> {
        let raw = self.storage.tload(Self::ADDRESS, Self::SENDER_OWNER_ID_SLOT)?;
        Ok(B256::from(raw.to_be_bytes::<32>()))
    }

    /// Writes the resolved transaction context into transient storage.
    ///
    /// Intended for the EIP-8130 execution layer to call once at the start of
    /// transaction processing. The values are cleared automatically when the
    /// transaction's transient storage is reset.
    pub fn set_context(
        &mut self,
        sender: Address,
        payer: Address,
        sender_owner_id: B256,
    ) -> Result<()> {
        self.storage.tstore(
            Self::ADDRESS,
            Self::SENDER_SLOT,
            U256::from_be_bytes(sender.into_word().0),
        )?;
        self.storage.tstore(
            Self::ADDRESS,
            Self::PAYER_SLOT,
            U256::from_be_bytes(payer.into_word().0),
        )?;
        self.storage.tstore(
            Self::ADDRESS,
            Self::SENDER_OWNER_ID_SLOT,
            U256::from_be_bytes(sender_owner_id.0),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, address, b256};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use crate::tx_context::storage::TxContextStorage;

    const SENDER: Address = address!("0x1111111111111111111111111111111111111111");
    const PAYER: Address = address!("0x2222222222222222222222222222222222222222");
    const SENDER_OWNER_ID: B256 =
        b256!("0x3333333333333333333333333333333333333333333333333333333333333333");

    #[test]
    fn context_is_zero_by_default() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            let view = TxContextStorage::new(ctx);
            assert_eq!(view.sender().unwrap(), Address::ZERO);
            assert_eq!(view.payer().unwrap(), Address::ZERO);
            assert_eq!(view.sender_owner_id().unwrap(), B256::ZERO);
        });
    }

    #[test]
    fn set_context_round_trips_each_field() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut view = TxContextStorage::new(ctx);
            view.set_context(SENDER, PAYER, SENDER_OWNER_ID).unwrap();

            assert_eq!(view.sender().unwrap(), SENDER);
            assert_eq!(view.payer().unwrap(), PAYER);
            assert_eq!(view.sender_owner_id().unwrap(), SENDER_OWNER_ID);
        });
    }

    #[test]
    fn context_clears_with_transient_storage() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).set_context(SENDER, PAYER, SENDER_OWNER_ID).unwrap();
            ctx.clear_transient();

            let view = TxContextStorage::new(ctx);
            assert_eq!(view.sender().unwrap(), Address::ZERO);
            assert_eq!(view.payer().unwrap(), Address::ZERO);
            assert_eq!(view.sender_owner_id().unwrap(), B256::ZERO);
        });
    }
}
