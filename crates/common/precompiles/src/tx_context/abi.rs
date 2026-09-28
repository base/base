//! ABI definitions for the EIP-8130 transaction context precompile.

use alloy_sol_types::sol;

sol! {
    /// Read-only EIP-8130 transaction context ABI.
    ///
    /// Exposes the resolved payer of the in-flight EIP-8130 transaction. On
    /// non-EIP-8130 transactions the backing transient slot is unset and the
    /// getter falls back to `tx.origin`.
    interface ITransactionContext {
        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// ETH was attached to a call targeting a nonpayable transaction-context selector.
        error NonPayable();

        /// Returns the resolved payer of the in-flight transaction.
        ///
        /// Equal to the sender when the transaction is self-paying.
        function getTransactionPayer() external view returns (address);
    }
}
