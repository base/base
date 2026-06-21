//! ABI definitions for the EIP-8130 2D nonce manager precompile.

use alloy_sol_types::sol;

sol! {
    /// EIP-8130 2D nonce manager ABI.
    ///
    /// Exposes the per-`(account, nonceKey)` sequence nonces that enable
    /// concurrent EIP-8130 transaction execution. Nonce key `0` is reserved for
    /// the protocol nonce and is stored in account state rather than here, so it
    /// is not readable through this precompile.
    interface INonceManager {
        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// Nonce key `0` is the protocol nonce and is not served by this precompile.
        error ProtocolNonceNotSupported();

        /// Nonce key `0` is reserved for the protocol nonce and cannot be incremented here.
        error InvalidNonceKey();

        /// The 2D nonce for the `(account, nonceKey)` channel is already at its maximum.
        error NonceOverflow();

        /// Expiring-nonce `validBefore` is outside the allowed `(now, now + maxExpiry]` window.
        error InvalidExpiringNonceExpiry();

        /// An expiring-nonce replay hash has already been recorded and has not yet expired.
        error ExpiringNonceReplay();

        /// The expiring-nonce ring buffer is full of unexpired entries and cannot accept more.
        error ExpiringNonceSetFull();

        /// Emitted when the 2D nonce for `(account, nonceKey)` is incremented to `newNonce`.
        event NonceIncremented(address indexed account, uint256 indexed nonceKey, uint64 newNonce);

        /// Returns the current 2D nonce for `account` at `nonceKey`.
        ///
        /// Reverts with `ProtocolNonceNotSupported` for nonce key `0`.
        function getNonce(address account, uint256 nonceKey) external view returns (uint64);
    }
}
