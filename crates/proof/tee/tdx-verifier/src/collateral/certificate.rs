//! Explicit verifier certificate inputs and authenticated X.509 fields.

use alloy_primitives::{B256, Bytes, keccak256};
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

use crate::{Result, TdxVerifierError};

use super::CollateralVerifier;

/// Certificate data supplied as explicit verifier input.
///
/// The verifier consumes the raw bytes for hashing and an explicit P-256
/// public key/signature envelope for deterministic ZK-guest-friendly chain
/// validation. Deployment code can construct this structure from DER X.509
/// collateral before entering the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxCertificate {
    /// Raw certificate bytes, hashed into journals and trust anchors.
    pub raw: Bytes,
    /// Certificate serial number used by revocation evidence.
    pub serial: Bytes,
    /// Uncompressed P-256 subject public key: `0x04 || x || y`.
    pub subject_public_key: Bytes,
    /// Uncompressed P-256 issuer public key: `0x04 || x || y`.
    pub issuer_public_key: Bytes,
    /// Certificate validity start time in seconds since Unix epoch.
    pub not_before: u64,
    /// Certificate validity end time in seconds since Unix epoch.
    pub not_after: u64,
    /// Whether this certificate may issue child certificates.
    pub is_ca: bool,
    /// DER-encoded `TBSCertificate` bytes covered by the X.509 signature.
    pub tbs_certificate: Bytes,
    /// P-256 ECDSA signature over [`Self::tbs_certificate`].
    pub signature: Bytes,
}

impl TdxCertificate {
    /// Builds a verifier certificate input from DER X.509 bytes.
    pub fn from_der(raw: Bytes, issuer_public_key: Bytes) -> Result<Self> {
        let authenticated = Self::authenticated_from_der(&raw)?;
        Ok(Self {
            raw,
            serial: authenticated.serial,
            subject_public_key: authenticated.subject_public_key,
            issuer_public_key,
            not_before: authenticated.not_before,
            not_after: authenticated.not_after,
            is_ca: authenticated.is_ca,
            tbs_certificate: authenticated.tbs_certificate,
            signature: authenticated.signature,
        })
    }

    /// Returns the contract-compatible hash of the raw certificate bytes.
    pub fn hash(&self) -> B256 {
        keccak256(&self.raw)
    }

    /// Verifies this certificate's signature with an issuer P-256 public key.
    pub fn verify_signature(&self, issuer_public_key: &[u8]) -> Result<()> {
        if self.tbs_certificate.is_empty() {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "certificate TBS bytes are empty".into(),
            ));
        }
        CollateralVerifier::verify_p256_signature(
            issuer_public_key,
            &self.tbs_certificate,
            &self.signature,
            TdxVerifierError::PckCertChainInvalid("certificate signature failed".into()),
        )
    }

    /// Parses and authenticates fields that must be sourced from DER X.509 bytes.
    pub fn authenticated_from_der(raw: &[u8]) -> Result<AuthenticatedTdxCertificate> {
        let (remaining, cert) = X509Certificate::from_der(raw).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("X.509 parse failed: {e}"))
        })?;
        if !remaining.is_empty() {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "certificate DER has trailing bytes".into(),
            ));
        }

        let not_before = u64::try_from(cert.validity().not_before.timestamp()).map_err(|_| {
            TdxVerifierError::PckCertChainInvalid("certificate notBefore is negative".into())
        })?;
        let not_after = u64::try_from(cert.validity().not_after.timestamp()).map_err(|_| {
            TdxVerifierError::PckCertChainInvalid("certificate notAfter is negative".into())
        })?;
        let basic_constraints = cert.basic_constraints().map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("basicConstraints parse failed: {e}"))
        })?;

        Ok(AuthenticatedTdxCertificate {
            serial: Bytes::copy_from_slice(cert.tbs_certificate.raw_serial()),
            issuer_name: Bytes::copy_from_slice(cert.tbs_certificate.issuer().as_raw()),
            subject_name: Bytes::copy_from_slice(cert.tbs_certificate.subject().as_raw()),
            subject_public_key: Bytes::copy_from_slice(
                cert.public_key().subject_public_key.data.as_ref(),
            ),
            not_before,
            not_after,
            is_ca: basic_constraints.map(|extension| extension.value.ca).unwrap_or(false),
            tbs_certificate: Bytes::copy_from_slice(cert.tbs_certificate.as_ref()),
            signature: Bytes::copy_from_slice(cert.signature_value.data.as_ref()),
        })
    }

    /// Verifies that explicit verifier fields match the authenticated DER certificate.
    pub fn verify_authenticated_fields(
        &self,
        authenticated: &AuthenticatedTdxCertificate,
    ) -> Result<()> {
        if self.serial != authenticated.serial
            || self.subject_public_key != authenticated.subject_public_key
            || self.not_before != authenticated.not_before
            || self.not_after != authenticated.not_after
            || self.is_ca != authenticated.is_ca
            || self.tbs_certificate != authenticated.tbs_certificate
            || self.signature != authenticated.signature
        {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "explicit certificate fields do not match DER certificate".into(),
            ));
        }
        Ok(())
    }
}

/// Certificate fields authenticated by DER X.509 parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedTdxCertificate {
    /// DER certificate serial number.
    pub serial: Bytes,
    /// DER-encoded issuer name.
    pub issuer_name: Bytes,
    /// DER-encoded subject name.
    pub subject_name: Bytes,
    /// Uncompressed P-256 subject public key: `0x04 || x || y`.
    pub subject_public_key: Bytes,
    /// Certificate validity start time in seconds since Unix epoch.
    pub not_before: u64,
    /// Certificate validity end time in seconds since Unix epoch.
    pub not_after: u64,
    /// Whether this certificate may issue child certificates.
    pub is_ca: bool,
    /// DER-encoded `TBSCertificate` bytes covered by the X.509 signature.
    pub tbs_certificate: Bytes,
    /// DER-encoded P-256 ECDSA signature over `tbs_certificate`.
    pub signature: Bytes,
}
