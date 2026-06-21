//! Successful outcome of EIP-8130 authenticator dispatch.

use alloy_primitives::{Address, B256};

/// The result of running an authenticator over a signing `hash` and auth blob.
///
/// Dispatch is the stateless "Authenticate" step; the stateful "Authorize" step
/// (`actor_config` lookup, scope, expiry, implicit-EOA rule) consumes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Authentication fully resolved to an `actorId` by a verifying authenticator
    /// (native secp256k1 ecrecover sentinel, P-256, or `WebAuthn`). The signature
    /// has been cryptographically verified against `hash`.
    Authenticated {
        /// The resolved actor id.
        actor_id: B256,
    },

    /// The delegate authenticator: the nested signature has been cryptographically
    /// verified, but the protocol must still **authorize** the nested actor against
    /// the delegated account's `actor_config` in SIGNATURE context. That stateful
    /// check is performed by the authorize stage; dispatch only proves the nested
    /// signature is valid and surfaces the obligation here.
    Delegated {
        /// Outer actor id registered on the originating account:
        /// `bytes32(bytes20(delegate_account))`.
        actor_id: B256,
        /// The delegated account (B) whose config the nested actor must be
        /// authorized against, in SIGNATURE context.
        delegate_account: Address,
        /// The nested (canonical, non-delegate) authenticator that verified the
        /// nested signature. The authorize stage must check this matches the
        /// nested actor's stored authenticator on `delegate_account`.
        nested_authenticator: Address,
        /// The nested actor id resolved by the nested authenticator.
        nested_actor_id: B256,
    },
}
