//! Staged rollout policy for user-signed validity predicates.

use std::{fmt, str::FromStr};

use alloy_primitives::Signature;

use crate::{ValidityAuthorizationError, ValidityPredicate};

/// Signature policy shared by forwarding ingress and builders.
///
/// Deploy `VerifyIfPresent` fleet-wide before migrating clients, then switch to
/// `Required`. Optional verification does not prevent signature-stripping attacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValiditySignatureMode {
    /// Accept unsigned or signed sidecars without verifying supplied signatures.
    #[default]
    Off,
    /// Accept unsigned predicates; raw ingress verifies every supplied signature.
    VerifyIfPresent,
    /// Require a sender signature for every non-empty batch; raw ingress verifies it.
    Required,
}

impl ValiditySignatureMode {
    /// Checks sidecar shape and signature presence before cryptographic verification.
    /// Trusted builder insert uses only this check. Raw ingress must also verify
    /// signatures with [`crate::ValidityAuthorization`].
    pub const fn check(
        self,
        predicates: &[ValidityPredicate],
        signature: Option<&Signature>,
    ) -> Result<(), ValidityAuthorizationError> {
        if matches!(self, Self::Off) {
            return Ok(());
        }
        if predicates.is_empty() {
            return if signature.is_some() {
                Err(ValidityAuthorizationError::UnexpectedSignature)
            } else {
                Ok(())
            };
        }
        match (self, signature.is_some()) {
            (Self::Required, false) => Err(ValidityAuthorizationError::MissingSignature),
            _ => Ok(()),
        }
    }
}

impl fmt::Display for ValiditySignatureMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::VerifyIfPresent => "verify-if-present",
            Self::Required => "required",
        })
    }
}

impl FromStr for ValiditySignatureMode {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "verify-if-present" => Ok(Self::VerifyIfPresent),
            "required" => Ok(Self::Required),
            _ => Err("expected off, verify-if-present, or required"),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Signature, U256};

    use super::ValiditySignatureMode;
    use crate::ValidityAuthorizationError;

    #[rstest::rstest]
    #[case::off(ValiditySignatureMode::Off, Ok(()))]
    #[case::optional(
        ValiditySignatureMode::VerifyIfPresent,
        Err(ValidityAuthorizationError::UnexpectedSignature)
    )]
    #[case::required(
        ValiditySignatureMode::Required,
        Err(ValidityAuthorizationError::UnexpectedSignature)
    )]
    fn signature_without_predicates_obeys_mode(
        #[case] mode: ValiditySignatureMode,
        #[case] expected: Result<(), ValidityAuthorizationError>,
    ) {
        let signature = Signature::new(U256::ZERO, U256::ZERO, false);
        assert_eq!(mode.check(&[], Some(&signature)), expected);
    }
}
