//! CRL revocation evidence parsing and certificate revocation checks.

use alloy_primitives::Bytes;
use x509_parser::prelude::{CertificateRevocationList, FromDer};

use crate::{Result, TdxVerifierError};

use super::{AuthenticatedTdxCertificate, CollateralVerifier, TdxCertificate};

/// DER X.509 CRL supplied as revocation evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxCertificateRevocationList {
    /// Raw DER-encoded X.509 certificate revocation list.
    pub raw: Bytes,
}

impl TdxCertificateRevocationList {
    /// Parses authenticated CRL fields from DER bytes.
    pub fn authenticated_from_der(raw: &[u8]) -> Result<AuthenticatedTdxCrl> {
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

        Ok(AuthenticatedTdxCrl {
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
    /// Verifies the CRL signature with the issuer's P-256 public key.
    pub fn verify_signature(&self, issuer_public_key: &[u8]) -> Result<()> {
        CollateralVerifier::verify_p256_signature(
            issuer_public_key,
            &self.tbs_cert_list,
            &self.signature,
            TdxVerifierError::PckCertChainInvalid("CRL signature failed".into()),
        )
    }

    /// Validates this CRL's time window at `verification_time`.
    pub fn verify_validity(&self, verification_time: u64) -> Result<()> {
        if verification_time < self.this_update || verification_time >= self.next_update {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "CRL is not valid at verification time".into(),
            ));
        }
        Ok(())
    }

    /// Returns true when this CRL revokes `certificate`.
    pub fn revokes_certificate(&self, certificate: &AuthenticatedTdxCertificate) -> bool {
        self.revoked_serials.iter().any(|serial| serial == &certificate.serial)
    }
}

/// Explicit signed revocation evidence supplied to the verifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TdxRevocationEvidence {
    /// DER X.509 CRLs for all non-root certificate issuers used by verification.
    pub certificate_crls: Vec<TdxCertificateRevocationList>,
}

impl TdxRevocationEvidence {
    /// Pre-parses all supplied CRLs into authenticated form for repeated lookups.
    pub fn authenticate_crls(&self) -> Result<Vec<AuthenticatedTdxCrl>> {
        self.certificate_crls
            .iter()
            .map(|crl| TdxCertificateRevocationList::authenticated_from_der(&crl.raw))
            .collect()
    }

    /// Verifies a certificate against pre-authenticated CRLs.
    pub fn verify_certificate_not_revoked_with_crls(
        authenticated_crls: &[AuthenticatedTdxCrl],
        certificate: &AuthenticatedTdxCertificate,
        issuer: &AuthenticatedTdxCertificate,
        verification_time: u64,
    ) -> Result<u64> {
        let mut found_issuer_crl = false;
        let mut earliest_next_update = u64::MAX;
        for authenticated in authenticated_crls {
            if authenticated.issuer_name != issuer.subject_name {
                continue;
            }
            found_issuer_crl = true;
            authenticated.verify_signature(&issuer.subject_public_key)?;
            authenticated.verify_validity(verification_time)?;
            earliest_next_update = earliest_next_update.min(authenticated.next_update);
            if authenticated.revokes_certificate(certificate) {
                return Err(TdxVerifierError::PckCertChainInvalid("certificate is revoked".into()));
            }
        }

        if !found_issuer_crl {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "missing issuer CRL for certificate".into(),
            ));
        }
        Ok(earliest_next_update)
    }

    /// Returns the earliest nextUpdate among CRLs used to validate a certificate chain.
    pub fn certificate_chain_next_update(
        &self,
        chain: &[TdxCertificate],
        verification_time: u64,
    ) -> Result<u64> {
        let authenticated_chain = chain
            .iter()
            .map(|cert| TdxCertificate::authenticated_from_der(&cert.raw))
            .collect::<Result<Vec<_>>>()?;
        let authenticated_crls = self.authenticate_crls()?;
        let mut earliest_next_update = u64::MAX;
        for index in 1..authenticated_chain.len() {
            earliest_next_update =
                earliest_next_update.min(Self::verify_certificate_not_revoked_with_crls(
                    &authenticated_crls,
                    &authenticated_chain[index],
                    &authenticated_chain[index - 1],
                    verification_time,
                )?);
        }
        Ok(earliest_next_update)
    }
}
