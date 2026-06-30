//! Raw verifier certificate inputs and authenticated X.509 fields.

use alloy_primitives::{B256, Bytes, keccak256};
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

use crate::{Result, TdxVerifierError};

/// Certificate data supplied as explicit verifier input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxCertificate {
    /// Raw certificate bytes, hashed into journals and trust anchors.
    pub raw: Bytes,
}

impl TdxCertificate {
    /// Returns the contract-compatible hash of the raw certificate bytes.
    pub fn hash(&self) -> B256 {
        keccak256(&self.raw)
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
