//! Errors returned by the EIP-8130 actor authorization step.

use alloy_primitives::{Address, B256};
use base_execution_eip8130::AuthError;
use base_precompile_storage::BasePrecompileError;

/// Reason an actor could not be authorized for an account.
///
/// Every variant is a hard rejection: the transaction MUST NOT be admitted or
/// included on the strength of this authentication. The on-chain
/// `AccountConfiguration._authenticate` reverts in each corresponding case.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorizeError {
    /// The stateless authenticate (dispatch) step failed: malformed blob,
    /// non-canonical or revoked authenticator, or an invalid signature.
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

    /// The resolved actor's configured expiry has passed (`now > expiry`).
    #[error("actor {actor_id} expired at {expiry}")]
    Expired {
        /// The resolved actor id that is expired.
        actor_id: B256,
        /// The Unix-seconds expiry that was exceeded.
        expiry: u64,
    },

    /// The nested actor in a delegate authentication lacks `SCOPE_SIGNATURE` on
    /// the delegate account. Mirrors `DelegateAuthenticator` requiring
    /// `verifySignature(delegate, ...)`, which accepts only an unrestricted
    /// (`scope == 0`) or `SCOPE_SIGNATURE` actor.
    #[error("delegate nested actor {actor_id} lacks SIGNATURE scope on the delegate account")]
    NestedSignatureScope {
        /// The nested actor id that failed the SIGNATURE-scope check.
        actor_id: B256,
    },

    /// The implicit-EOA self-actor slot is occupied by an explicit actor, so the
    /// implicit owner is shadowed and `address(0)` cannot authenticate. Mirrors
    /// `require(_actorConfig[self][account].authenticator == address(0))`.
    #[error("implicit-EOA self-actor slot is occupied by an explicit actor")]
    ImplicitEoaShadowed,

    /// The implicit-EOA signature recovered an address other than the account
    /// itself. Mirrors `require(recovered == account)`.
    #[error("implicit-EOA signer does not match the account")]
    ImplicitEoaMismatch,
}
