//! Append-only business-logic interface for the B-20 token factory precompile.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::Result;

use crate::{B20FactoryStorage, IB20Factory};

/// The B-20 token factory logic interface.
///
/// This trait is append-only: new versions add methods, never remove or change the
/// signature of an existing one.
pub trait Factory {
    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`.
    ///
    /// `address_hash` must be `keccak256(abi_encode(caller, call.salt))`. Computing (and
    /// metering) that hash is the dispatcher's responsibility; this method only consumes
    /// the result. `upgrade` selects the policy-logic version the created token is bound to.
    ///
    /// Defaults to borrowing `call`'s fields into [`Self::create_b20_decoded`] rather than
    /// owning a separate implementation, so callers that already hold an owned
    /// [`IB20Factory::createB20Call`] (only non-ABI-dispatch callers reach this; every ABI
    /// dispatch takes [`Self::create_b20_decoded`] directly) keep working unchanged.
    fn create_b20(
        &self,
        storage: &mut B20FactoryStorage<'_>,
        call: IB20Factory::createB20Call,
        address_hash: B256,
        upgrade: BaseUpgrade,
    ) -> Result<Address> {
        let init_calls: Vec<&[u8]> = call.initCalls.iter().map(Bytes::as_ref).collect();
        self.create_b20_decoded(
            storage,
            call.variant,
            call.params.as_ref(),
            &init_calls,
            address_hash,
            upgrade,
        )
    }

    /// Creates a token from already-decoded `createB20` fields, borrowed rather than owned.
    ///
    /// Takes no `salt`: the dispatcher folds it into `address_hash` before calling this method,
    /// and nothing below needs the raw value. Borrowed `params`/`init_calls` let the dispatcher
    /// decode straight from calldata without copying. This is the primary entry point;
    /// [`Self::create_b20`] is a thin owned-call convenience built on top of it.
    ///
    /// Removal (`alloy-aliasing`): drop this method and give `create_b20` back the owned body,
    /// taking `params: &Bytes` and `init_calls: Vec<Bytes>`.
    fn create_b20_decoded(
        &self,
        storage: &mut B20FactoryStorage<'_>,
        variant: IB20Factory::B20Variant,
        params: &[u8],
        init_calls: &[&[u8]],
        address_hash: B256,
        upgrade: BaseUpgrade,
    ) -> Result<Address>;

    // --- version-invariant reads: default pass-throughs to `B20FactoryStorage` ---

    /// Returns whether `token` has the structural B-20 prefix.
    fn is_b20(&self, storage: &B20FactoryStorage<'_>, token: Address) -> Result<bool> {
        storage.is_b20(token)
    }

    /// Returns whether `token` is a B-20 address that has been initialized by this factory.
    fn is_b20_initialized(&self, storage: &B20FactoryStorage<'_>, token: Address) -> Result<bool> {
        storage.is_b20_initialized(token)
    }
}
