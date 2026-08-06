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
    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`.
    ///
    /// `address_hash` must be `keccak256(abi_encode(caller, call.salt))`. Computing (and
    /// metering) that hash is the dispatcher's responsibility; this method only consumes
    /// the result. `upgrade` selects the policy-logic version the created token is bound to.
    fn create_b20(
        &self,
        storage: &mut B20FactoryStorage<'_>,
        call: IB20Factory::createB20Call,
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
