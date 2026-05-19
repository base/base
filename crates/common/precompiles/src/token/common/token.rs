use alloy_primitives::Address;

use super::TokenAccounting;

/// Token identity layer, bridging the storage port to capability traits.
///
/// `Token` provides two things:
/// - Accessors to the underlying storage ([`Self::accounting`] /
///   [`Self::accounting_mut`]) that all capability trait default impls use to
///   read and write state without the 22-method delegation block.
/// - [`Self::token_address`], the on-chain address of this token.
///
/// All capability traits extend `Token`. Implement it on a token struct by
/// wiring the `accounting` field and delegating address identity to the backing storage.
///
/// The associated type `Accounting` is resolved at compile time, so all
/// storage calls in the capability traits are monomorphized — no vtable
/// overhead on the hot path.
pub trait Token {
    /// The concrete storage adapter backing this token.
    type Accounting: TokenAccounting;

    /// Returns a shared reference to this token's storage adapter.
    fn accounting(&self) -> &Self::Accounting;
    /// Returns an exclusive reference to this token's storage adapter.
    fn accounting_mut(&mut self) -> &mut Self::Accounting;
    /// Returns the on-chain address of this token contract.
    fn token_address(&self) -> Address;
}
