//! Shared collateral validation helpers.

use alloy_primitives::{B256, Bytes};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use x509_parser::{certificate::X509Certificate, extensions::ParsedExtension, prelude::FromDer};

use crate::{ParsedTdxQuote, Result, TdxVerifierError};

use super::{
    AuthenticatedTdxCertificate, AuthenticatedTdxCrl, INTEL_TCB_SIGNING_CERT_COMMON_NAME,
    TdxCertificate, TdxRevocationEvidence, TdxSignedCollateral, TdxSignedCollateralBody,
};

/// Stateless helper for collateral validation.
#[derive(Debug)]
pub struct CollateralVerifier;

impl CollateralVerifier {
    /// Validates a root-to-leaf certificate chain and returns the leaf key plus CRL expiration.
    pub fn verify_certificate_chain(
        chain: &[TdxCertificate],
        trusted_root_ca_hash: B256,
        verification_time: u64,
        revocation: &TdxRevocationEvidence,
    ) -> Result<(Bytes, u64)> {
        let root = chain.first().ok_or_else(|| {
            TdxVerifierError::PckCertChainInvalid("certificate chain is empty".into())
        })?;
        if root.hash() != trusted_root_ca_hash {
            return Err(TdxVerifierError::RootCaNotTrusted);
        }

        let authenticated_chain = chain
            .iter()
            .map(|cert| TdxCertificate::authenticated_from_der(&cert.raw))
            .collect::<Result<Vec<_>>>()?;
        let authenticated_crls = revocation
            .certificate_crls
            .iter()
            .map(|crl| AuthenticatedTdxCrl::authenticated_from_der(crl))
            .collect::<Result<Vec<_>>>()?;
        let mut crl_expiration = u64::MAX;

        for (index, authenticated) in authenticated_chain.iter().enumerate() {
            if verification_time < authenticated.not_before
                || verification_time >= authenticated.not_after
            {
                return Err(TdxVerifierError::PckCertChainInvalid(
                    "certificate is not valid at verification time".into(),
                ));
            }
            if index == 0 {
                Self::verify_certificate_signature(
                    authenticated,
                    &authenticated.subject_public_key,
                )?;
                continue;
            }

            let issuer = &authenticated_chain[index - 1];
            if !issuer.is_ca {
                return Err(TdxVerifierError::PckCertChainInvalid(
                    "issuer certificate is not a CA".into(),
                ));
            }
            Self::verify_certificate_signature(authenticated, &issuer.subject_public_key)?;
            if authenticated.issuer_name != issuer.subject_name {
                return Err(TdxVerifierError::PckCertChainInvalid(
                    "certificate issuer name does not match parent".into(),
                ));
            }
            crl_expiration = crl_expiration.min(
                TdxRevocationEvidence::verify_certificate_not_revoked_with_crls(
                    &authenticated_crls,
                    authenticated,
                    issuer,
                    verification_time,
                )?,
            );
        }

        Ok((
            authenticated_chain.last().expect("chain is non-empty").subject_public_key.clone(),
            crl_expiration,
        ))
    }

    /// Verifies an authenticated certificate signature with an issuer P-256 public key.
    pub fn verify_certificate_signature(
        certificate: &AuthenticatedTdxCertificate,
        issuer_public_key: &[u8],
    ) -> Result<()> {
        if certificate.tbs_certificate.is_empty() {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "certificate TBS bytes are empty".into(),
            ));
        }
        Self::verify_p256_signature(
            issuer_public_key,
            &certificate.tbs_certificate,
            &certificate.signature,
            TdxVerifierError::PckCertChainInvalid("certificate signature failed".into()),
        )
    }

    /// Validates signed collateral and returns its leaf signing key plus CRL expiration.
    pub fn verify_signed_collateral(
        collateral: &TdxSignedCollateral,
        body_kind: TdxSignedCollateralBody,
        trusted_root_ca_hash: B256,
        verification_time: u64,
        revocation: &TdxRevocationEvidence,
        error_mapper: fn(String) -> TdxVerifierError,
    ) -> Result<(Bytes, u64)> {
        let (issue_time, next_update) = collateral.signed_validity(body_kind)?;
        if collateral.issue_time != issue_time || collateral.next_update != next_update {
            return Err(error_mapper(
                "explicit collateral validity does not match signed JSON".into(),
            ));
        }
        if verification_time < issue_time || verification_time >= next_update {
            return Err(TdxVerifierError::CollateralExpired);
        }

        let (leaf_key, crl_expiration) = Self::verify_certificate_chain(
            &collateral.signing_chain,
            trusted_root_ca_hash,
            verification_time,
            revocation,
        )
        .map_err(|e| match e {
            TdxVerifierError::RootCaNotTrusted => TdxVerifierError::RootCaNotTrusted,
            other => error_mapper(other.to_string()),
        })?;
        let leaf = collateral
            .signing_chain
            .last()
            .ok_or_else(|| error_mapper("collateral signing chain is empty".into()))?;
        Self::verify_collateral_signing_certificate(leaf, error_mapper)?;

        let signed_body = collateral.signed_body_bytes(body_kind)?;
        Self::verify_p256_signature(
            &leaf_key,
            &signed_body,
            &collateral.signature,
            error_mapper("collateral signature failed".into()),
        )?;

        Ok((leaf_key, crl_expiration))
    }

    /// Verifies that a collateral leaf is the expected Intel PCS TCB signing certificate.
    pub fn verify_collateral_signing_certificate(
        certificate: &TdxCertificate,
        error_mapper: fn(String) -> TdxVerifierError,
    ) -> Result<()> {
        let (remaining, cert) = X509Certificate::from_der(&certificate.raw).map_err(|e| {
            error_mapper(format!("collateral signing certificate parse failed: {e}"))
        })?;
        if !remaining.is_empty() {
            return Err(error_mapper(
                "collateral signing certificate DER has trailing bytes".into(),
            ));
        }

        let mut common_names = cert.subject().iter_common_name();
        let common_name =
            common_names.next().and_then(|name| name.as_str().ok()).ok_or_else(|| {
                error_mapper("collateral signing certificate is missing subject common name".into())
            })?;
        if common_names.next().is_some() || common_name != INTEL_TCB_SIGNING_CERT_COMMON_NAME {
            return Err(error_mapper(
                "collateral signing certificate subject is not Intel TCB Signing".into(),
            ));
        }

        let basic_constraints = cert.basic_constraints().map_err(|e| {
            error_mapper(format!("collateral signing basicConstraints parse failed: {e}"))
        })?;
        if basic_constraints.map(|extension| extension.value.ca).unwrap_or(false) {
            return Err(error_mapper("collateral signing certificate must not be a CA".into()));
        }

        let has_digital_signature_usage =
            cert.tbs_certificate.extensions().iter().any(|extension| {
                matches!(
                    extension.parsed_extension(),
                    ParsedExtension::KeyUsage(key_usage) if key_usage.digital_signature()
                )
            });
        if !has_digital_signature_usage {
            return Err(error_mapper(
                "collateral signing certificate is missing digitalSignature key usage".into(),
            ));
        }

        Ok(())
    }

    /// Verifies a raw P-256 ECDSA signature over `message`.
    pub fn verify_p256_signature(
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
        error: TdxVerifierError,
    ) -> Result<()> {
        let verifying_key = VerifyingKey::from_sec1_bytes(public_key).map_err(|e| {
            TdxVerifierError::PckCertChainInvalid(format!("invalid P-256 public key: {e}"))
        })?;
        let signature =
            match Signature::from_slice(signature).or_else(|_| Signature::from_der(signature)) {
                Ok(signature) => signature,
                Err(e) => return Err(error.with_message(format!("{e}"))),
            };
        verifying_key.verify(message, &signature).map_err(|_| error)?;
        Ok(())
    }

    /// Parses an RFC3339 timestamp into Unix seconds.
    pub fn parse_rfc3339_seconds(value: &str) -> std::result::Result<u64, String> {
        let timestamp = OffsetDateTime::parse(value, &Rfc3339)
            .map_err(|e| format!("RFC3339 parse failed: {e}"))?
            .unix_timestamp();
        u64::try_from(timestamp).map_err(|_| "timestamp is negative".into())
    }

    /// Decodes hex text, accepting an optional `0x` prefix.
    pub fn decode_hex(value: &str) -> std::result::Result<Bytes, String> {
        let value = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
        hex::decode(value).map(Bytes::from).map_err(|e| e.to_string())
    }

    /// Decodes hex text and enforces a specific byte length.
    pub fn decode_hex_exact(
        value: &str,
        expected_len: usize,
    ) -> std::result::Result<Bytes, String> {
        let decoded = Self::decode_hex(value)?;
        if decoded.len() != expected_len {
            return Err(format!(
                "hex string length {} does not match expected {expected_len}",
                decoded.len()
            ));
        }
        Ok(decoded)
    }

    /// Returns true when masked bytes match expected hex text.
    pub fn masked_bytes_match(
        actual: &[u8],
        expected_hex: &str,
        mask_hex: &str,
    ) -> std::result::Result<bool, String> {
        let expected = Self::decode_hex_exact(expected_hex, actual.len())?;
        let mask = Self::decode_hex_exact(mask_hex, actual.len())?;
        Ok(actual
            .iter()
            .zip(expected.iter())
            .zip(mask.iter())
            .all(|((actual, expected), mask)| actual & mask == expected & mask))
    }

    /// Verifies common TDX module identity fields against the quote report body.
    pub fn verify_module_identity_fields(
        quote: &ParsedTdxQuote,
        mrsigner: &str,
        attributes: &str,
        attributes_mask: &str,
    ) -> Result<()> {
        let expected_mrsigner = Self::decode_hex_exact(mrsigner, quote.mrsigner_seam.len())
            .map_err(TdxVerifierError::TcbInfoInvalid)?;
        if quote.mrsigner_seam.as_slice() != expected_mrsigner.as_ref() {
            return Err(TdxVerifierError::TcbInfoInvalid(
                "TDX module signer does not match quote MRSIGNERSEAM".into(),
            ));
        }
        let attributes_match =
            Self::masked_bytes_match(&quote.seam_attributes, attributes, attributes_mask)
                .map_err(TdxVerifierError::TcbInfoInvalid)?;
        if !attributes_match {
            return Err(TdxVerifierError::TcbInfoInvalid(
                "TDX module attributes do not match quote SEAM attributes".into(),
            ));
        }
        Ok(())
    }
}
