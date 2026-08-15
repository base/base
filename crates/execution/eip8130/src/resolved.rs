//! The authorization surface returned by a successful authorize step.

use alloy_primitives::{Address, B256};

/// A resolved and authorized actor: the output of
/// [`ActorAuthorizer::authenticate_actor`](crate::ActorAuthorizer::authenticate_actor),
/// mirroring `AccountConfiguration.authenticateActor`'s return tuple.
///
/// Authorization is **scope + policy**, not scope alone: `scope` is the actor's
/// capability set and `policy_type`/`policy_target` describe its policy gate. The
/// consuming validator combines these with the transaction's operation (sender,
/// payer, or config change) to make the final scope/policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedActor {
    /// The resolved actor id (`bytes20(address)` for ecrecover/delegate,
    /// `keccak256(x‖y)` for P-256/`WebAuthn`).
    pub actor_id: B256,
    /// The actor's scope bitfield (`0 = unrestricted`).
    pub scope: u8,
    /// The actor's policy sub-type byte (`0 = ungated`).
    pub policy_type: u8,
    /// The actor's policy gate target (the policy *manager*), or
    /// [`Address::ZERO`] when ungated. Never the signed policy commitment.
    pub policy_target: Address,
}

impl ResolvedActor {
    /// An unrestricted, ungated owner — the surface of an implicit EOA and the
    /// shape of any actor with `scope == 0` and no policy.
    #[must_use]
    pub const fn unrestricted(actor_id: B256) -> Self {
        Self { actor_id, scope: 0, policy_type: 0, policy_target: Address::ZERO }
    }

    /// `true` if the actor carries no elevated scope (`scope == 0`).
    #[must_use]
    pub const fn is_unrestricted(&self) -> bool {
        self.scope == 0
    }
}
