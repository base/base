//! Per-operation scope gating for resolved actors, including admin-only config
//! changes and policy-gated sender authorization.

use base_common_consensus::Eip8130Constants;

use crate::ResolvedActor;

/// The transaction context an actor is being authorized for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Authorizing the transaction sender (`SCOPE_SENDER`).
    Sender,
    /// Authorizing a gas payer (`SCOPE_PAYER`).
    Payer,
    /// Authorizing an account-configuration change (admin only).
    Config,
    /// Authorizing a message signature, ERC-1271 style (`SCOPE_SIGNATURE`).
    Signature,
}

impl Operation {
    /// The scope bit that grants this operation, or unrestricted scope for
    /// admin-only configuration changes.
    #[must_use]
    pub const fn required_bit(self) -> u8 {
        match self {
            Self::Sender => Eip8130Constants::SCOPE_SENDER,
            Self::Payer => Eip8130Constants::SCOPE_PAYER,
            Self::Config => Eip8130Constants::SCOPE_UNRESTRICTED,
            Self::Signature => Eip8130Constants::SCOPE_SIGNATURE,
        }
    }

    /// Whether `scope` grants this operation.
    #[must_use]
    pub const fn is_granted_by(self, scope: u8) -> bool {
        match self {
            Self::Config => scope == Eip8130Constants::SCOPE_UNRESTRICTED,
            Self::Sender => {
                if scope == Eip8130Constants::SCOPE_UNRESTRICTED {
                    return true;
                }
                if scope & Eip8130Constants::SCOPE_POLICY != 0
                    && scope & Eip8130Constants::SCOPE_SIGNATURE != 0
                {
                    return false;
                }
                scope & Eip8130Constants::SCOPE_SENDER != 0
                    || scope & Eip8130Constants::SCOPE_POLICY != 0
            }
            Self::Payer => {
                scope == Eip8130Constants::SCOPE_UNRESTRICTED
                    || scope & Eip8130Constants::SCOPE_PAYER != 0
            }
            Self::Signature => {
                scope == Eip8130Constants::SCOPE_UNRESTRICTED
                    || scope & Eip8130Constants::SCOPE_SIGNATURE != 0
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
    fn sender_accepts_sender_or_policy_except_policy_signature() {
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_UNRESTRICTED));
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_SENDER));
        assert!(Operation::Sender.is_granted_by(Eip8130Constants::SCOPE_POLICY));
        assert!(
            !Operation::Sender
                .is_granted_by(Eip8130Constants::SCOPE_POLICY | Eip8130Constants::SCOPE_SIGNATURE)
        );
    }
}
