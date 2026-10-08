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
    /// Preserve legacy unsigned predicates and reject signed sidecars.
    #[default]
    Off,
    /// Accept unsigned predicates, but verify every supplied signature.
    VerifyIfPresent,
    /// Require a valid sender signature for every non-empty predicate batch.
    Required,
}

impl ValiditySignatureMode {
    /// Checks sidecar shape and signature presence before cryptographic verification.
    /// This does not verify signatures; use [`crate::ValidityAuthorization`] for admission.
    pub const fn check(
        self,
        predicates: &[ValidityPredicate],
        signature: Option<&Signature>,
    ) -> Result<(), ValidityAuthorizationError> {
        if predicates.is_empty() {
            return if signature.is_some() {
                Err(ValidityAuthorizationError::UnexpectedSignature)
            } else {
                Ok(())
            };
        }
        match (self, signature.is_some()) {
            (Self::Off, true) => Err(ValidityAuthorizationError::Disabled),
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
