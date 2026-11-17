//! Stateless structural rules for an EIP-8130 transaction.
//!
//! Shared by pool admission and RPC simulation, so a request the pool would
//! reject as malformed is not priced or simulated either.

use super::{Eip8130Constants, Eip8130Signed};

/// Why an EIP-8130 transaction fails the stateless structural rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Eip8130StructuralError {
    /// More call phases than [`Eip8130Constants::MAX_CALL_PHASES_PER_TX`].
    #[error("call phase count exceeds maximum")]
    TooManyCallPhases,
    /// `sender_auth` has the wrong shape for its path or names an authenticator
    /// other than [`Eip8130Constants::K1_AUTHENTICATOR`].
    #[error("malformed sender authorization")]
    MalformedSenderAuth,
    /// `payer_auth` is missing, unexpected, or has the wrong shape for its mode.
    #[error("malformed payer authorization")]
    MalformedPayerAuth,
    /// More account changes than
    /// [`Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX`].
    #[error("malformed account change")]
    MalformedAccountChange,
}

/// The stateless structural rules for an EIP-8130 transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip8130Structure;

impl Eip8130Structure {
    /// Length of a raw secp256k1 signature, `r || s || v`.
    const K1_SIGNATURE_LEN: usize = 65;

    /// Checks every structural rule: the call phase count, the sender and payer
    /// authorization shapes, and the account changes.
    pub fn validate(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        if signed.tx().calls.len() > Eip8130Constants::MAX_CALL_PHASES_PER_TX {
            return Err(Eip8130StructuralError::TooManyCallPhases);
        }
        Self::validate_sender_auth(signed)?;
        Self::validate_payer_auth(signed)?;
        Self::validate_account_changes(signed)
    }

    /// Checks `sender_auth` is a raw 65-byte signature when the sender is
    /// recovered, and `K1_AUTHENTICATOR || r || s || v` when it is named.
    pub fn validate_sender_auth(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        let auth = signed.sender_auth();
        let well_formed = if signed.explicit_sender().is_none() {
            auth.len() == Self::K1_SIGNATURE_LEN
        } else {
            Self::is_named_k1(auth)
        };
        if well_formed { Ok(()) } else { Err(Eip8130StructuralError::MalformedSenderAuth) }
    }

    /// Ensures `payer_auth` is present iff a `payer` is set. Open payer mode
    /// carries a raw 65-byte signature; a named payer carries
    /// `K1_AUTHENTICATOR || r || s || v`.
    pub fn validate_payer_auth(signed: &Eip8130Signed) -> Result<(), Eip8130StructuralError> {
        let auth = signed.payer_auth();
        let well_formed = match signed.tx().payer {
            None => auth.is_empty(),
            Some(_) if signed.tx().is_open_payer() => auth.len() == Self::K1_SIGNATURE_LEN,
            Some(_) => Self::is_named_k1(auth),
        };
        if well_formed { Ok(()) } else { Err(Eip8130StructuralError::MalformedPayerAuth) }
    }

    /// Allows at most [`Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX`] account
    /// changes.
    pub const fn validate_account_changes(
        signed: &Eip8130Signed,
    ) -> Result<(), Eip8130StructuralError> {
        if signed.tx().account_changes.len() > Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX {
            return Err(Eip8130StructuralError::MalformedAccountChange);
        }
        Ok(())
    }

    /// Whether `auth` is `K1_AUTHENTICATOR || r || s || v`.
    fn is_named_k1(auth: &[u8]) -> bool {
        auth.strip_prefix(Eip8130Constants::K1_AUTHENTICATOR.as_slice())
            .is_some_and(|signature| signature.len() == Self::K1_SIGNATURE_LEN)
    }
}
