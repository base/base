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

    /// The delegate authenticator: dispatch has only *structurally* parsed the
    /// blob (delegate account + single-hop check). The nested signature is **not**
    /// verified here; the authorize stage must run the full `authenticateActor`
    /// path against the delegated account (inline default-EOA k1 self *or* an
    /// explicit `actor_config` entry) and require admin (`scope == 0`). This
    /// mirrors the deployed `DelegateAuthenticator`, which itself only calls
    /// `ACCOUNT_CONFIGURATION.authenticateActor(delegate, ...)`.
    Delegated {
        /// Outer actor id registered on the originating account:
        /// `bytes32(bytes20(delegate_account))`.
        actor_id: B256,
        /// The delegated account (B) whose config the nested actor must be
        /// authorized against via `authenticateActor`.
        delegate_account: Address,
    },
}
