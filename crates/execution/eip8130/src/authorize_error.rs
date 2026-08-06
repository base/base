//! Errors returned by the EIP-8130 actor authorization step.

use alloy_primitives::{Address, B256};
use base_precompile_storage::BasePrecompileError;

use crate::AuthError;

/// Reason an actor could not be authorized for an account.
///
/// Every variant is a hard rejection: the transaction MUST NOT be admitted or
/// included on the strength of this authentication. The on-chain
/// `AccountConfiguration._authenticate` reverts in each corresponding case.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorizeError {
    /// The stateless authenticate (dispatch) step failed: malformed blob,
    /// non-canonical authenticator, or an invalid signature.
    #[error("authenticate failed: {0}")]
    Authenticate(#[from] AuthError),

    /// A storage read against the `AccountConfiguration` contract failed.
    #[error("account-configuration read failed: {0}")]
    Storage(#[from] BasePrecompileError),

    /// The resolved actor id was zero (e.g. a delegate to `address(0)`). Mirrors
    /// the contract's `require(actorId != bytes32(0))`.
    #[error("resolved actor id is zero")]
    ZeroActor,

    /// The resolved actor is not bound to the authenticator that signed for it on
    /// this account (`actor_config.authenticator != authenticator`), so the actor
    /// is unknown, revoked, or registered under a different authenticator.
    #[error("actor {actor_id} is not bound to authenticator {authenticator} on the account")]
    NotBound {
        /// The resolved actor id whose binding check failed.
        actor_id: B256,
        /// The authenticator that signed and was expected to be bound.
        authenticator: Address,
    },

    /// A k1 signature recovered to the account itself, but the account's
    /// secp256k1 self key is disabled: its `DEFAULT_EOA_REVOKED` flag is set, so
    /// the self key has been revoked outright or superseded by a non-k1 self
    /// authenticator. Mirrors `_authenticateK1`'s `require(flag unset)` on the
    /// `recovered == account` path.
    #[error("secp256k1 self key is disabled for account {account}")]
    DefaultEoaRevoked {
        /// The account whose inline self key is disabled.
        account: Address,
    },

    /// The resolved actor's configured expiry has passed (`now > expiry`).
    #[error("actor {actor_id} expired at {expiry}")]
    Expired {
        /// The resolved actor id that is expired.
        actor_id: B256,
        /// The Unix-seconds expiry that was exceeded.
        expiry: u64,
    },

    /// The nested actor in a delegate authentication is not admin on the
    /// delegate account. Mirrors `DelegateAuthenticator`, which calls
    /// `authenticateActor(delegate, ...)` and explicitly requires the resolved
    /// `scope == 0`. This is independent of `verifySignature` (operational
    /// signing): a delegate vouch requires admin to preserve non-escalation.
    #[error("delegate nested actor {actor_id} is not admin on the delegate account")]
    NestedSignatureScope {
        /// The nested actor id that failed the admin-only delegate-vouch check.
        actor_id: B256,
    },
}
