//! The authorization surface returned by a successful authorize step.

use alloy_primitives::{B256, U256};
use base_common_consensus::Eip8130Constants;

/// A resolved and authorized actor: the output of
/// [`ActorAuthorizer::authenticate_actor`](crate::ActorAuthorizer::authenticate_actor),
/// mirroring `AccountConfiguration.authenticateActor`'s return tuple.
///
/// `scope` is the actor's capability set; the consuming validator combines it
/// with the transaction's operation (sender, payer, or config change) to make
/// the final scope decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedActor {
    /// The resolved actor id (`bytes32(uint256(uint160(address)))`, right-aligned,
    /// for ecrecover/delegate; `keccak256(x‖y)` for P-256/`WebAuthn`).
    pub actor_id: B256,
    /// The actor's scope bitfield (`uint16`; `0 = unrestricted`).
    pub scope: u16,
    /// The actor's Unix-seconds authorization expiry (`0 = no expiry`). The
    /// authorization is valid while `now <= expiry`; surfaced so the mempool can
    /// evict transactions that depend on a key whose authorization expires before
    /// inclusion (a wall-clock surface no storage diff reports).
    pub expiry: u64,
}

impl ResolvedActor {
    /// An unrestricted owner — the surface of an implicit EOA and the shape of
    /// any actor with `scope == 0`.
    #[must_use]
    pub const fn unrestricted(actor_id: B256) -> Self {
        Self { actor_id, scope: 0, expiry: 0 }
    }

    /// `true` if the actor is an unrestricted administrator (`scope == 0`).
    ///
    /// EIP-8130 defines these as the same concept: there is no separate admin
    /// grant bit and no restricted administrator.
    #[must_use]
    pub const fn is_admin(&self) -> bool {
        self.scope == 0
    }

    /// Whether this actor may use the transaction's nonce key.
    ///
    /// Unrestricted actors and nonce-free transactions are always allowed;
    /// scoped actors need `SCOPE_NONCE` for sequenced nonce channels.
    #[must_use]
    pub fn can_use_nonce_key(&self, nonce_key: U256) -> bool {
        self.scope == Eip8130Constants::SCOPE_UNRESTRICTED
            || nonce_key == Eip8130Constants::NONCE_KEY_MAX
            || self.scope & Eip8130Constants::SCOPE_NONCE != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(scope: u16) -> ResolvedActor {
        ResolvedActor { actor_id: B256::ZERO, scope, expiry: 0 }
    }

    #[test]
    fn nonce_scope_allows_expected_keys() {
        assert!(actor(0).can_use_nonce_key(U256::ZERO));
        assert!(
            actor(Eip8130Constants::SCOPE_OPERATOR)
                .can_use_nonce_key(Eip8130Constants::NONCE_KEY_MAX)
        );
        assert!(actor(Eip8130Constants::SCOPE_NONCE).can_use_nonce_key(U256::from(1)));
        assert!(!actor(Eip8130Constants::SCOPE_OPERATOR).can_use_nonce_key(U256::ZERO));
    }
}
