use alloy_primitives::Address;

use crate::{Policy, TokenAccounting};

/// Token identity layer, bridging the storage port to capability traits.
///
/// `Token` provides three things:
/// - Accessors to the underlying storage ([`Self::accounting`] /
///   [`Self::accounting_mut`]) that all capability trait default impls use to
///   read and write state without the 22-method delegation block.
/// - Accessors to the global policy registry ([`Self::policy`] /
///   [`Self::policy_mut`]) for policy decisions shared across all tokens.
/// - [`Self::token_address`], the on-chain address of this token.
///
/// All capability traits extend `Token`. Implement it on a token struct by
/// wiring the `accounting` and `policy` fields and delegating address identity to the backing storage.
///
/// The associated types `Accounting` and `Policy` are resolved at compile
/// time, so all storage and policy calls in the capability traits are
/// monomorphized — no vtable overhead on the hot path.
pub trait Token {
    /// The concrete storage adapter backing this token.
    type Accounting: TokenAccounting;
    /// The global policy registry precompile backing this token.
    type Policy: Policy;

    /// Returns a shared reference to this token's storage adapter.
    fn accounting(&self) -> &Self::Accounting;
    /// Returns an exclusive reference to this token's storage adapter.
    fn accounting_mut(&mut self) -> &mut Self::Accounting;
    /// Returns a shared reference to the global policy registry.
    fn policy(&self) -> &Self::Policy;
    /// Returns an exclusive reference to the global policy registry.
    fn policy_mut(&mut self) -> &mut Self::Policy;
    /// Returns the on-chain address of this token contract.
    fn token_address(&self) -> Address;
}
