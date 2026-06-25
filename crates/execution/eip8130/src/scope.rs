//! Per-operation scope gating for resolved actors.

use base_common_consensus::Eip8130Constants;

use crate::ResolvedActor;

/// The transaction context an actor is being authorized for. Each maps to the
/// EIP-8130 scope bit that gates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Operation {
    /// Authorizing the transaction sender (`SCOPE_SENDER`).
    Sender,
    /// Authorizing a gas payer (`SCOPE_PAYER`).
    Payer,
    /// Authorizing an account-configuration change (`SCOPE_CONFIG`).
    Config,
    /// Authorizing a message signature, ERC-1271 style (`SCOPE_SIGNATURE`).
    Signature,
}

impl Operation {
    /// The scope bit that grants this operation.
    #[must_use]
    pub const fn required_bit(self) -> u8 {
        match self {
            Self::Sender => Eip8130Constants::SCOPE_SENDER,
            Self::Payer => Eip8130Constants::SCOPE_PAYER,
            Self::Config => Eip8130Constants::SCOPE_CONFIG,
            Self::Signature => Eip8130Constants::SCOPE_SIGNATURE,
        }
    }

    /// Whether `scope` grants this operation. An unrestricted actor
    /// (`scope == 0`) is valid in every context; otherwise the operation's bit
    /// must be set. Mirrors `AccountConfiguration`'s
    /// `scope == 0 || scope & REQUIRED != 0`.
    #[must_use]
    pub const fn is_granted_by(self, scope: u8) -> bool {
        scope == Eip8130Constants::SCOPE_UNRESTRICTED || scope & self.required_bit() != 0
    }

    /// Whether the resolved actor's scope grants this operation.
    #[must_use]
    pub const fn is_granted(self, actor: &ResolvedActor) -> bool {
        self.is_granted_by(actor.scope)
    }
}
