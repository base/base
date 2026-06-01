//! ABI definitions for the EIP-8130 transaction context precompile.

use alloy_sol_types::sol;

sol! {
    /// Read-only EIP-8130 transaction context ABI.
    ///
    /// Exposes the resolved sender, payer, and sender owner id for the
    /// in-flight EIP-8130 transaction. On non-EIP-8130 transactions the
    /// backing transient slots are unset and the getters return the zero
    /// address / zero word.
    interface ITxContext {
        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// Returns the resolved sender of the in-flight transaction.
        function getSender() external view returns (address);

        /// Returns the resolved payer of the in-flight transaction.
        ///
        /// Equal to the sender when the transaction is self-paying.
        function getPayer() external view returns (address);

        /// Returns the owner id resolved while authenticating the sender.
        function getSenderOwnerId() external view returns (bytes32);
    }
}
