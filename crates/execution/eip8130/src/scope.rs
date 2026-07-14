//! Per-operation scope gating for resolved actors, including admin-only config
//! changes and policy-gated sender authorization.

use base_common_consensus::Eip8130Constants;

use crate::ResolvedActor;

/// The transaction context an actor is being authorized for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Authorizing the transaction sender (`SCOPE_SENDER` or `SCOPE_POLICY`).
    Sender,
    /// Authorizing an actor to pay the account's own gas when `payer == sender`
    /// (`SCOPE_SELF_PAYER`).
    SelfPayer,
    /// Authorizing an actor to sponsor a different sender's gas
    /// (`payer != sender`, `SCOPE_SPONSOR_PAYER`).
    SponsorPayer,
    /// Authorizing an account-configuration change (admin only, `scope == 0`).
    Config,
}

impl Operation {
    /// The scope bit that grants this operation, or unrestricted scope for
    /// admin-only configuration changes.
    #[must_use]
    pub const fn required_bit(self) -> u8 {
        match self {
            Self::Sender => Eip8130Constants::SCOPE_SENDER,
            Self::SelfPayer => Eip8130Constants::SCOPE_SELF_PAYER,
            Self::SponsorPayer => Eip8130Constants::SCOPE_SPONSOR_PAYER,
            Self::Config => Eip8130Constants::SCOPE_UNRESTRICTED,
        }
    }

    /// Whether `scope` grants this operation.
    #[must_use]
    pub const fn is_granted_by(self, scope: u8) -> bool {
        match self {
            Self::Config => scope == Eip8130Constants::SCOPE_UNRESTRICTED,
            Self::Sender => {
                scope == Eip8130Constants::SCOPE_UNRESTRICTED
                    || scope & Eip8130Constants::SCOPE_SENDER != 0
                    || scope & Eip8130Constants::SCOPE_POLICY != 0
            }
            Self::SelfPayer => {
                scope == Eip8130Constants::SCOPE_UNRESTRICTED
                    || scope & Eip8130Constants::SCOPE_SELF_PAYER != 0
            }
            Self::SponsorPayer => {
                scope == Eip8130Constants::SCOPE_UNRESTRICTED
                    || scope & Eip8130Constants::SCOPE_SPONSOR_PAYER != 0
            }
        }
    }

    /// Whether the resolved actor's scope grants this operation.
    #[must_use]
    pub const fn is_granted(self, actor: &ResolvedActor) -> bool {
        self.is_granted_by(actor.scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_admin_only() {
        assert!(Operation::Config.is_granted_by(Eip8130Constants::SCOPE_UNRESTRICTED));
        assert!(!Operation::Config.is_granted_by(Eip8130Constants::SCOPE_SENDER));
        assert!(!Operation::Config.is_granted_by(Eip8130Constants::SCOPE_POLICY));
    }

    #[test]
    fn sender_accepts_sender_or_policy() {
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_UNRESTRICTED));
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_SENDER));
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_POLICY));
        assert!(!Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_SELF_PAYER));
    }

    #[test]
    fn payer_grants_are_split_by_self_vs_sponsor() {
        // Self-pay requires SELF_PAYER (or admin); sponsor requires SPONSOR_PAYER.
        assert!(Operation::SelfPayer.is_granted_by(Eip8130Constants::SCOPE_UNRESTRICTED));
        assert!(Operation::SelfPayer.is_granted_by(Eip8130Constants::SCOPE_SELF_PAYER));
        assert!(!Operation::SelfPayer.is_granted_by(Eip8130Constants::SCOPE_SPONSOR_PAYER));

        assert!(Operation::SponsorPayer.is_granted_by(Eip8130Constants::SCOPE_UNRESTRICTED));
        assert!(Operation::SponsorPayer.is_granted_by(Eip8130Constants::SCOPE_SPONSOR_PAYER));
        assert!(!Operation::SponsorPayer.is_granted_by(Eip8130Constants::SCOPE_SELF_PAYER));
    }
}
