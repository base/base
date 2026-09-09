use alloy_primitives::Address;

use crate::{PolicyAccounting, PolicyRegistryLogic, TokenAccounting};

/// Token identity layer, bridging the storage port to capability traits.
///
/// `Token` provides:
/// - Accessors to the underlying storage ([`Self::accounting`] /
///   [`Self::accounting_mut`]) that all capability trait default impls use to
///   read and write state without the 22-method delegation block.
/// - Access to the policy registry: [`Self::policy`] returns the active version's
///   [`PolicyRegistryLogic`] contract, and [`Self::policy_storage`] /
///   [`Self::policy_storage_mut`] expose the storage it operates on. Together they
///   make the whole policy contract available at the call site, e.g.
///   `token.policy().is_authorized(token.policy_storage(), policy_id, account)`.
/// - [`Self::token_address`], the on-chain address of this token.
///
/// All capability traits extend `Token`. Implement it on a token struct by
/// wiring the `accounting` and `policy` fields and delegating address identity to the backing storage.
///
/// The associated types are resolved at compile time, so storage and policy calls
/// in the capability traits are monomorphized — no vtable overhead on the hot path.
pub trait Token {
    /// The concrete storage adapter backing this token.
    type Accounting: TokenAccounting;
    /// The policy-registry storage port backing this token's authorization checks.
    type PolicyAccounting: PolicyAccounting;

    /// Returns a shared reference to this token's storage adapter.
    fn accounting(&self) -> &Self::Accounting;
    /// Returns an exclusive reference to this token's storage adapter.
    fn accounting_mut(&mut self) -> &mut Self::Accounting;
    /// Returns the policy-registry logic contract active for this token's version.
    fn policy(&self) -> &dyn PolicyRegistryLogic<Self::PolicyAccounting>;
    /// Returns a shared reference to the policy-registry storage the contract reads from.
    fn policy_storage(&self) -> &Self::PolicyAccounting;
    /// Returns an exclusive reference to the policy-registry storage port.
    fn policy_storage_mut(&mut self) -> &mut Self::PolicyAccounting;
    /// Returns the on-chain address of this token contract.
    fn token_address(&self) -> Address;
}
