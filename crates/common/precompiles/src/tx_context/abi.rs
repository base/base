//! ABI definitions for the EIP-8130 transaction context precompile.

use alloy_sol_types::sol;

sol! {
    /// Read-only EIP-8130 transaction context ABI.
    ///
    /// Exposes the resolved sender, payer, and sender actor id for the
    /// in-flight EIP-8130 transaction. On non-EIP-8130 transactions the
    /// backing transient slots are unset and the getters fall back to the
    /// ambient origin: `getTransactionSender` and `getTransactionPayer`
    /// return `tx.origin`, and `getTransactionSenderActorId` returns
    /// `bytes32(bytes20(tx.origin))` (the address left-aligned in the high
    /// 20 bytes).
    interface ITransactionContext {
        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// Returns the resolved sender of the in-flight transaction.
        function getTransactionSender() external view returns (address);

        /// Returns the resolved payer of the in-flight transaction.
        ///
        /// Equal to the sender when the transaction is self-paying.
        function getTransactionPayer() external view returns (address);

        /// Returns the actor id resolved while authenticating the sender.
        function getTransactionSenderActorId() external view returns (bytes32);
    }
}
