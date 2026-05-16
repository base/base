use alloy_primitives::Address;

use super::ITokenCoreAccounting;

/// Token identity layer, bridging the storage port to capability traits.
///
/// `IToken` provides two things:
/// - Accessors to the underlying storage ([`Self::accounting`] /
///   [`Self::accounting_mut`]) that all capability trait default impls use to
///   read and write state without the 22-method delegation block.
/// - [`Self::token_address`], the fixed on-chain address of this token.
///
/// All capability traits extend `IToken`. Implement it on a token struct by
/// wiring the `accounting` field and providing the precompile address.
pub trait IToken {
    /// Returns a shared reference to this token's storage adapter.
    fn accounting(&self) -> &dyn ITokenCoreAccounting;
    /// Returns an exclusive reference to this token's storage adapter.
    fn accounting_mut(&mut self) -> &mut dyn ITokenCoreAccounting;
    /// Returns the on-chain address of this token contract.
    fn token_address(&self) -> Address;
}




