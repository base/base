//! The authorization surface returned by a successful authorize step.

use alloy_primitives::{Address, B256, U256};
use base_common_consensus::Eip8130Constants;

/// A resolved and authorized actor: the output of
/// [`ActorAuthorizer::authenticate_actor`](crate::ActorAuthorizer::authenticate_actor),
/// mirroring `AccountConfiguration.authenticateActor`'s return tuple.
///
/// Authorization is **scope + policy**, not scope alone: `scope` is the actor's
/// capability set and `policy_target` describes its policy gate. The
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
    /// The actor's policy gate target (the policy *manager*), or
    /// [`Address::ZERO`] when ungated. Never the signed policy commitment.
    pub policy_target: Address,
}

impl ResolvedActor {
    /// An unrestricted, ungated owner — the surface of an implicit EOA and the
    /// shape of any actor with `scope == 0` and no policy.
    #[must_use]
    pub const fn unrestricted(actor_id: B256) -> Self {
        Self { actor_id, scope: 0, policy_target: Address::ZERO }
    }

    /// `true` if the actor is an unrestricted administrator (`scope == 0`).
    ///
    /// EIP-8130 defines these as the same concept: there is no separate admin
    /// grant bit and no restricted administrator.
    #[must_use]
    pub const fn is_admin(&self) -> bool {
        self.scope == 0
    }

    /// `true` if the actor's sender authorization is policy-gated.
    #[must_use]
    pub const fn is_policy_gated(&self) -> bool {
        self.scope & Eip8130Constants::SCOPE_POLICY != 0
    }

    /// Whether this actor may use the transaction's nonce key.
    ///
    /// Unrestricted actors and nonce-free transactions are always allowed;
    /// scoped actors need `SCOPE_NONCE` for sequenced nonce channels.
    #[must_use]
    pub fn allows_sequenced_nonce(&self, nonce_key: U256) -> bool {
        self.scope == Eip8130Constants::SCOPE_UNRESTRICTED
            || nonce_key == Eip8130Constants::NONCE_KEY_MAX
            || self.scope & Eip8130Constants::SCOPE_NONCE != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(scope: u8) -> ResolvedActor {
        ResolvedActor { actor_id: B256::ZERO, scope, policy_target: Address::ZERO }
    }

    #[test]
    fn nonce_scope_allows_expected_keys() {
        assert!(actor(0).allows_sequenced_nonce(U256::ZERO));
        assert!(
            actor(Eip8130Constants::SCOPE_SENDER)
                .allows_sequenced_nonce(Eip8130Constants::NONCE_KEY_MAX)
        );
        assert!(actor(Eip8130Constants::SCOPE_NONCE).allows_sequenced_nonce(U256::from(1)));
        assert!(!actor(Eip8130Constants::SCOPE_SENDER).allows_sequenced_nonce(U256::ZERO));
    }
}
