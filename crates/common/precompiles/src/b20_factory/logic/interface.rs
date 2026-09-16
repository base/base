//! Append-only business-logic interface for the B-20 token factory precompile.

use alloy_primitives::{Address, B256};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::Result;

use crate::{B20FactoryStorage, IB20Factory};

/// The B-20 token factory logic interface.
///
/// This trait is append-only: new versions add methods, never remove or change the
/// signature of an existing one.
pub trait Factory {
    /// Creates a token from already-decoded `createB20` fields, borrowed rather than owned.
    ///
    /// `address_hash` must be `keccak256(abi_encode(caller, salt))`. Computing (and metering) that
    /// hash is the dispatcher's responsibility; this method only consumes the result, so it takes
    /// no `salt`. `upgrade` selects the policy-logic version the created token is bound to. Borrowed
    /// `params`/`init_calls` let the dispatcher decode straight from calldata without copying;
    /// `B20FactoryStorage::create_b20` is a thin owned-call convenience that borrows into this.
    ///
    /// Removal (`alloy-aliasing`): restore an owned `create_b20` taking `params: &Bytes` and
    /// `init_calls: Vec<Bytes>`, and revert `B20FactoryStorage::create_b20` to call it.
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
