//! Storage layout and constants for the EIP-8130 transaction context precompile.

use alloy_primitives::{Address, B256, U256, address};
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};

/// Transient-storage-backed view of the in-flight EIP-8130 transaction context.
///
/// The resolved sender, payer, and sender actor id are written to transient
/// storage at [`Self::ADDRESS`] by the EIP-8130 execution layer at the start of
/// transaction processing and cleared automatically at transaction end. The
/// precompile only reads them back.
///
/// For any non-EIP-8130 transaction (where nothing is written) each getter falls
/// back to `tx.origin`: [`Self::sender`] and [`Self::payer`] return the origin
/// address and [`Self::sender_actor_id`] returns `bytes32(bytes20(tx.origin))`,
/// so callers observe the actual transaction originator uniformly across tx types.
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

    /// Transient slot holding the resolved sender address.
    const SENDER_SLOT: U256 = U256::ZERO;
    /// Transient slot holding the resolved payer address.
    const PAYER_SLOT: U256 = U256::from_limbs([1, 0, 0, 0]);
    /// Transient slot holding the sender actor id.
    const SENDER_ACTOR_ID_SLOT: U256 = U256::from_limbs([2, 0, 0, 0]);

    /// Creates a transaction context view over the active storage scope.
    pub const fn new(storage: StorageCtx<'a>) -> Self {
        Self { storage }
    }

    /// Returns the resolved sender, falling back to `tx.origin` when unset
    /// (i.e. outside an EIP-8130 transaction).
    pub fn sender(&self) -> Result<Address> {
        let raw = self.storage.tload(Self::ADDRESS, Self::SENDER_SLOT)?;
        if raw.is_zero() {
            return Ok(self.storage.origin());
        }
        Ok(Address::from_word(B256::from(raw.to_be_bytes::<32>())))
    }

    /// Returns the resolved payer, falling back to `tx.origin` when unset
    /// (i.e. outside an EIP-8130 transaction).
    pub fn payer(&self) -> Result<Address> {
        let raw = self.storage.tload(Self::ADDRESS, Self::PAYER_SLOT)?;
        if raw.is_zero() {
            return Ok(self.storage.origin());
        }
        Ok(Address::from_word(B256::from(raw.to_be_bytes::<32>())))
    }

    /// Returns the sender actor id, falling back to `bytes32(bytes20(tx.origin))`
    /// when unset (i.e. outside an EIP-8130 transaction).
    pub fn sender_actor_id(&self) -> Result<B256> {
        let raw = self.storage.tload(Self::ADDRESS, Self::SENDER_ACTOR_ID_SLOT)?;
        if raw.is_zero() {
            return Ok(Self::address_to_actor_id(self.storage.origin()));
        }
        Ok(B256::from(raw.to_be_bytes::<32>()))
    }

    /// Derives the EOA actor id for `addr` as `bytes32(bytes20(addr))`: the 20
    /// address bytes occupy the high-order bytes of the word, low 12 bytes zero
    /// (left-aligned, matching the EIP-8130 Solidity cast — distinct from the
    /// right-aligned EVM address word produced by [`Address::into_word`]).
    fn address_to_actor_id(addr: Address) -> B256 {
        let mut word = [0u8; 32];
        word[..20].copy_from_slice(addr.as_slice());
        B256::from(word)
    }

    /// Writes the resolved transaction context into transient storage.
    ///
    /// Intended for the EIP-8130 execution layer to call once at the start of
    /// transaction processing. The values are cleared automatically when the
    /// transaction's transient storage is reset.
    ///
    /// # Invariant
    /// In the EIP-8130 domain the resolved sender, payer, and actor id are always
    /// non-zero (sender/payer are recovered addresses; the actor id derives from
    /// the sender). The getters depend on this: a zero slot unambiguously means
    /// "unset" and selects the `tx.origin` fallback, so writing a zero field here
    /// would make it indistinguishable from an unset one and silently
    /// misattribute the transaction. This identity boundary is enforced at
    /// runtime in all builds (see Errors), not just via debug assertions.
    ///
    /// # Errors
    /// Returns [`BasePrecompileError::assert_failed`] if `sender`, `payer`, or
    /// `sender_actor_id` is zero, failing the transaction rather than corrupting
    /// sender/payer attribution.
    ///
    /// # Atomicity
    /// The three `tstore` writes are grouped under a storage checkpoint, so a
    /// mid-write failure (e.g. out-of-gas between writes) reverts the whole group
    /// rather than leaving a half-set context (e.g. a real sender alongside an
    /// unset payer that then falls back to `tx.origin`). Transient storage also
    /// resets at transaction end, so nothing persists across transactions.
    pub fn set_context(
        &mut self,
        sender: Address,
        payer: Address,
        sender_actor_id: B256,
    ) -> Result<()> {
        // Identity boundary: a zero field is indistinguishable from an unset
        // transient slot and would silently misattribute the transaction to
        // tx.origin via the getter fallback. Reject it at runtime (in all builds)
        // so a buggy caller fails the transaction rather than corrupting
        // sender/payer attribution.
        if sender.is_zero() || payer.is_zero() || sender_actor_id.is_zero() {
            return Err(BasePrecompileError::assert_failed());
        }
        // Write the three correlated context slots atomically: the guard reverts
        // on drop unless committed, so a failure between writes leaves no
        // half-set context.
        let checkpoint = self.storage.checkpoint();
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
            Self::SENDER_ACTOR_ID_SLOT,
            U256::from_be_bytes(sender_actor_id.0),
        )?;
        checkpoint.commit();
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
    const SENDER_ACTOR_ID: B256 =
        b256!("0x3333333333333333333333333333333333333333333333333333333333333333");
    const ORIGIN: Address = address!("0x9999999999999999999999999999999999999999");
    /// `bytes32(bytes20(ORIGIN))`: address left-aligned in the high 20 bytes.
    const ORIGIN_ACTOR_ID: B256 =
        b256!("0x9999999999999999999999999999999999999999000000000000000000000000");

    #[test]
    fn context_is_zero_when_origin_is_zero() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            let view = TxContextStorage::new(ctx);
            assert_eq!(view.sender().unwrap(), Address::ZERO);
            assert_eq!(view.payer().unwrap(), Address::ZERO);
            assert_eq!(view.sender_actor_id().unwrap(), B256::ZERO);
        });
    }

    #[test]
    fn context_falls_back_to_origin_when_unset() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            let view = TxContextStorage::new(ctx);
            assert_eq!(view.sender().unwrap(), ORIGIN);
            assert_eq!(view.payer().unwrap(), ORIGIN);
            assert_eq!(view.sender_actor_id().unwrap(), ORIGIN_ACTOR_ID);
        });
    }

    #[test]
    fn set_context_overrides_origin_fallback() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut view = TxContextStorage::new(ctx);
            view.set_context(SENDER, PAYER, SENDER_ACTOR_ID).unwrap();

            assert_eq!(view.sender().unwrap(), SENDER);
            assert_eq!(view.payer().unwrap(), PAYER);
            assert_eq!(view.sender_actor_id().unwrap(), SENDER_ACTOR_ID);
        });
    }

    #[test]
    fn set_context_rejects_zero_fields() {
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut view = TxContextStorage::new(ctx);
            assert!(view.set_context(Address::ZERO, PAYER, SENDER_ACTOR_ID).is_err());
            assert!(view.set_context(SENDER, Address::ZERO, SENDER_ACTOR_ID).is_err());
            assert!(view.set_context(SENDER, PAYER, B256::ZERO).is_err());
        });
    }

    #[test]
    fn context_clears_to_origin_with_transient_storage() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_origin(ORIGIN);

        StorageCtx::enter(&mut storage, |ctx| {
            TxContextStorage::new(ctx).set_context(SENDER, PAYER, SENDER_ACTOR_ID).unwrap();
            ctx.clear_transient();

            let view = TxContextStorage::new(ctx);
            assert_eq!(view.sender().unwrap(), ORIGIN);
            assert_eq!(view.payer().unwrap(), ORIGIN);
            assert_eq!(view.sender_actor_id().unwrap(), ORIGIN_ACTOR_ID);
        });
    }
}
