//! CRL revocation evidence parsing and certificate revocation checks.

use alloy_primitives::Bytes;
use x509_parser::prelude::{CertificateRevocationList, FromDer};

use crate::{Result, TdxVerifierError};

use super::{AuthenticatedTdxCertificate, CollateralVerifier};

/// Authenticated CRL fields parsed from DER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedTdxCrl {
    /// DER-encoded issuer name.
    pub issuer_name: Bytes,
    /// CRL issue time in seconds since Unix epoch.
    pub this_update: u64,
    /// CRL expiration time in seconds since Unix epoch.
    pub next_update: u64,
    /// Certificate serials revoked by this CRL.
    pub revoked_serials: Vec<Bytes>,
    /// DER-encoded `TBSCertList` bytes covered by the CRL signature.
    pub tbs_cert_list: Bytes,
    /// P-256 ECDSA signature over `tbs_cert_list`.
    pub signature: Bytes,
}

impl AuthenticatedTdxCrl {
    /// Parses authenticated CRL fields from DER bytes.
    pub fn authenticated_from_der(raw: &[u8]) -> Result<Self> {
        let (remaining, crl) = CertificateRevocationList::from_der(raw)
            .map_err(|e| TdxVerifierError::PckCertChainInvalid(format!("CRL parse failed: {e}")))?;
        if !remaining.is_empty() {
            return Err(TdxVerifierError::PckCertChainInvalid("CRL DER has trailing bytes".into()));
        }

        let this_update = u64::try_from(crl.last_update().timestamp()).map_err(|_| {
            TdxVerifierError::PckCertChainInvalid("CRL thisUpdate is negative".into())
        })?;
        let next_update = crl
            .next_update()
            .ok_or_else(|| {
                TdxVerifierError::PckCertChainInvalid("CRL nextUpdate is missing".into())
            })
            .and_then(|next_update| {
                u64::try_from(next_update.timestamp()).map_err(|_| {
                    TdxVerifierError::PckCertChainInvalid("CRL nextUpdate is negative".into())
                })
            })?;

        Ok(Self {
            issuer_name: Bytes::copy_from_slice(crl.issuer().as_raw()),
            this_update,
            next_update,
            revoked_serials: crl
                .iter_revoked_certificates()
                .map(|revoked| Bytes::copy_from_slice(revoked.raw_serial()))
                .collect(),
            tbs_cert_list: Bytes::copy_from_slice(crl.tbs_cert_list.as_ref()),
            signature: Bytes::copy_from_slice(crl.signature_value.data.as_ref()),
        })
    }
}

/// Explicit signed revocation evidence supplied to the verifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TdxRevocationEvidence {
    /// DER X.509 CRLs for all non-root certificate issuers used by verification.
    pub certificate_crls: Vec<Bytes>,
}

impl TdxRevocationEvidence {
    /// Verifies a certificate against pre-authenticated CRLs.
    pub fn verify_certificate_not_revoked_with_crls(
        authenticated_crls: &[AuthenticatedTdxCrl],
        certificate: &AuthenticatedTdxCertificate,
        issuer: &AuthenticatedTdxCertificate,
        verification_time: u64,
    ) -> Result<u64> {
        let mut earliest_next_update: Option<u64> = None;
        for authenticated in authenticated_crls {
            if authenticated.issuer_name != issuer.subject_name {
                continue;
            }
            CollateralVerifier::verify_p256_signature(
                &issuer.subject_public_key,
                &authenticated.tbs_cert_list,
                &authenticated.signature,
                TdxVerifierError::PckCertChainInvalid,
                TdxVerifierError::PckCertChainInvalid("CRL signature failed".into()),
            )?;
            if verification_time < authenticated.this_update
                || verification_time >= authenticated.next_update
            {
                return Err(TdxVerifierError::PckCertChainInvalid(
                    "CRL is not valid at verification time".into(),
                ));
            }
            earliest_next_update =
                Some(earliest_next_update.unwrap_or(u64::MAX).min(authenticated.next_update));
            if authenticated.revoked_serials.iter().any(|serial| serial == &certificate.serial) {
                return Err(TdxVerifierError::PckCertChainInvalid("certificate is revoked".into()));
            }
        }

        earliest_next_update.ok_or_else(|| {
            TdxVerifierError::PckCertChainInvalid("missing issuer CRL for certificate".into())
        })
    }
}
