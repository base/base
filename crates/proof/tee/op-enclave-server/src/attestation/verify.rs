//! Attestation document verification.
//!
//! This module provides verification of AWS Nitro Enclave attestation documents,
//! similar to the `nitrite` Go library.

use aws_nitro_enclaves_cose::CoseSign1;
use openssl::asn1::Asn1Time;
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::{X509, X509StoreContext};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::SystemTime;
use x509_cert::Certificate;
use x509_cert::der::Decode;

use crate::attestation::ca_roots::get_default_ca_root;
use crate::error::{AttestationError, ServerError};

/// An attestation document from a Nitro Enclave.
#[derive(Debug, Clone, Deserialize)]
pub struct AttestationDocument {
    /// Module ID
    pub module_id: String,
    /// Digest algorithm
    pub digest: String,
    /// Timestamp
    pub timestamp: u64,
    /// PCR values (index -> value)
    pub pcrs: BTreeMap<u16, serde_bytes::ByteBuf>,
    /// Certificate chain
    pub certificate: serde_bytes::ByteBuf,
    /// CA bundle
    #[serde(default)]
    pub cabundle: Vec<serde_bytes::ByteBuf>,
    /// Optional public key
    #[serde(default)]
    pub public_key: Option<serde_bytes::ByteBuf>,
    /// Optional user data
    #[serde(default)]
    pub user_data: Option<serde_bytes::ByteBuf>,
    /// Optional nonce
    #[serde(default)]
    pub nonce: Option<serde_bytes::ByteBuf>,
}

/// Result of verifying an attestation document.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// The verified attestation document.
    pub document: AttestationDocument,
    /// The certificate chain used for verification.
    pub certificate_chain: Vec<Certificate>,
}

/// Options for attestation verification.
#[derive(Debug, Clone, Default)]
pub struct VerifyOptions {
    /// The time to use for certificate validity checking.
    /// If None, the current system time is used.
    pub current_time: Option<SystemTime>,
}

impl VerifyOptions {
    /// Create new verify options with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the time to use for certificate validity checking.
    pub const fn with_time(mut self, time: SystemTime) -> Self {
        self.current_time = Some(time);
        self
    }
}

/// Verify that a certificate is valid at the given time.
///
/// Checks that the current time is between notBefore and notAfter.
fn check_certificate_validity(cert: &X509, check_time: &Asn1Time) -> Result<(), ServerError> {
    let not_before = cert.not_before();
    let not_after = cert.not_after();

    // Check if certificate is not yet valid
    if check_time < not_before {
        return Err(AttestationError::CertificateNotYetValid {
            not_before: not_before.to_string(),
        }
        .into());
    }

    // Check if certificate has expired
    if check_time > not_after {
        return Err(AttestationError::CertificateExpired {
            not_after: not_after.to_string(),
        }
        .into());
    }

    Ok(())
}

/// Verify the certificate chain against the AWS CA root.
///
/// This builds an X509 store with the CA root and intermediates,
/// then verifies the leaf certificate chains to the root.
fn verify_certificate_chain(
    leaf_cert: &X509,
    intermediates: &[X509],
    ca_root: &X509,
    check_time: Option<&Asn1Time>,
) -> Result<(), ServerError> {
    // Build the X509 store with the CA root
    let mut store_builder = X509StoreBuilder::new()
        .map_err(|e| AttestationError::X509StoreError(format!("failed to create store: {e}")))?;

    store_builder
        .add_cert(ca_root.clone())
        .map_err(|e| AttestationError::X509StoreError(format!("failed to add CA root: {e}")))?;

    // Note: OpenSSL X509StoreBuilder doesn't support setting verification time directly.
    // Certificate validity is checked separately in check_certificate_validity().
    let _ = check_time; // Suppress unused warning

    let store = store_builder.build();

    // Build the intermediate certificate chain
    let mut chain = Stack::new()
        .map_err(|e| AttestationError::X509StoreError(format!("failed to create stack: {e}")))?;

    for intermediate in intermediates {
        chain.push(intermediate.clone()).map_err(|e| {
            AttestationError::X509StoreError(format!("failed to add intermediate: {e}"))
        })?;
    }

    // Create a store context and verify the chain
    let mut store_ctx = X509StoreContext::new()
        .map_err(|e| AttestationError::X509StoreError(format!("failed to create context: {e}")))?;

    let result = store_ctx
        .init(&store, leaf_cert, &chain, |ctx| ctx.verify_cert())
        .map_err(|e| {
            AttestationError::ChainVerificationFailed(format!("verification init failed: {e}"))
        })?;

    if !result {
        let error = store_ctx.error().error_string().to_string();
        return Err(AttestationError::ChainVerificationFailed(error).into());
    }

    Ok(())
}

/// Verify an attestation document.
///
/// This verifies the COSE signature and certificate chain against the AWS CA roots.
/// Uses the current system time for certificate validity checking.
pub fn verify_attestation(attestation_bytes: &[u8]) -> Result<VerificationResult, ServerError> {
    verify_attestation_with_options(attestation_bytes, &VerifyOptions::default())
}

/// Verify an attestation document with custom options.
///
/// This verifies the COSE signature and certificate chain against the AWS CA roots.
/// The `options` parameter allows specifying a custom time for certificate validity checking,
/// which is useful for testing with recorded attestation documents.
pub fn verify_attestation_with_options(
    attestation_bytes: &[u8],
    options: &VerifyOptions,
) -> Result<VerificationResult, ServerError> {
    // Parse the COSE_Sign1 structure
    let cose_sign1 = CoseSign1::from_bytes(attestation_bytes)
        .map_err(|e| AttestationError::CoseVerify(format!("failed to parse COSE: {e:?}")))?;

    // Get the payload (the attestation document)
    let payload = cose_sign1
        .get_payload::<aws_nitro_enclaves_cose::crypto::Openssl>(None)
        .map_err(|e| AttestationError::CoseVerify(format!("failed to get COSE payload: {e:?}")))?;

    // Parse the CBOR attestation document
    let document: AttestationDocument = ciborium::from_reader(&payload[..])
        .map_err(|e| AttestationError::CborParse(e.to_string()))?;

    // Parse the certificate chain using x509-cert for the result
    let mut certificate_chain = Vec::new();

    // First, parse the leaf certificate using x509-cert
    let leaf_cert = Certificate::from_der(&document.certificate)
        .map_err(|e| AttestationError::CertificateChain(format!("invalid leaf cert: {e}")))?;
    certificate_chain.push(leaf_cert);

    // Parse intermediate certificates from cabundle
    for cert_der in &document.cabundle {
        let cert = Certificate::from_der(cert_der)
            .map_err(|e| AttestationError::CertificateChain(format!("invalid ca cert: {e}")))?;
        certificate_chain.push(cert);
    }

    // Parse certificates using OpenSSL for verification
    let openssl_leaf = X509::from_der(&document.certificate)
        .map_err(|e| AttestationError::CertificateChain(format!("openssl parse error: {e}")))?;

    let mut openssl_intermediates = Vec::new();
    for cert_der in &document.cabundle {
        let cert = X509::from_der(cert_der).map_err(|e| {
            AttestationError::CertificateChain(format!("openssl intermediate parse error: {e}"))
        })?;
        openssl_intermediates.push(cert);
    }

    // Get the public key from the certificate
    let public_key = openssl_leaf
        .public_key()
        .map_err(|e| AttestationError::CertificateChain(format!("missing public key: {e}")))?;

    // Verify the COSE signature
    cose_sign1
        .verify_signature::<aws_nitro_enclaves_cose::crypto::Openssl>(&public_key)
        .map_err(|e| {
            AttestationError::CoseVerify(format!("signature verification failed: {e:?}"))
        })?;

    // Get the CA root for chain verification
    let ca_root = get_default_ca_root()?;

    // Determine the time to use for validity checking
    let check_time = match options.current_time {
        Some(time) => {
            let duration = time
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|e| AttestationError::Verification(format!("invalid time: {e}")))?;
            Asn1Time::from_unix(duration.as_secs() as i64).map_err(|e| {
                AttestationError::Verification(format!("failed to create ASN1 time: {e}"))
            })?
        }
        None => Asn1Time::days_from_now(0).map_err(|e| {
            AttestationError::Verification(format!("failed to get current time: {e}"))
        })?,
    };

    // Check certificate validity times
    check_certificate_validity(&openssl_leaf, &check_time)?;
    for intermediate in &openssl_intermediates {
        check_certificate_validity(intermediate, &check_time)?;
    }
    check_certificate_validity(&ca_root.openssl_cert, &check_time)?;

    // Verify the certificate chain against the CA root
    verify_certificate_chain(
        &openssl_leaf,
        &openssl_intermediates,
        &ca_root.openssl_cert,
        Some(&check_time),
    )?;

    Ok(VerificationResult {
        document,
        certificate_chain,
    })
}

/// Verify an attestation document and check that PCR0 matches the expected value.
pub fn verify_attestation_with_pcr0(
    attestation_bytes: &[u8],
    expected_pcr0: &[u8],
) -> Result<VerificationResult, ServerError> {
    verify_attestation_with_pcr0_and_options(
        attestation_bytes,
        expected_pcr0,
        &VerifyOptions::default(),
    )
}

/// Verify an attestation document with custom options and check that PCR0 matches.
pub fn verify_attestation_with_pcr0_and_options(
    attestation_bytes: &[u8],
    expected_pcr0: &[u8],
    options: &VerifyOptions,
) -> Result<VerificationResult, ServerError> {
    let result = verify_attestation_with_options(attestation_bytes, options)?;

    // Check PCR0
    let pcr0 = result
        .document
        .pcrs
        .get(&0)
        .ok_or_else(|| AttestationError::MissingField("PCR0".to_string()))?;

    if pcr0.as_ref() != expected_pcr0 {
        return Err(AttestationError::Pcr0Mismatch.into());
    }

    Ok(result)
}

/// Extract the public key from an attestation document.
pub fn extract_public_key(document: &AttestationDocument) -> Result<Vec<u8>, ServerError> {
    document
        .public_key
        .as_ref()
        .map(|pk| pk.to_vec())
        .ok_or_else(|| AttestationError::MissingField("public_key".to_string()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::extension::{BasicConstraints, KeyUsage};
    use openssl::x509::{X509Builder, X509NameBuilder};
    use std::time::Duration;

    /// Helper to create a self-signed test certificate with custom validity period
    fn create_test_cert_with_times(not_before: Asn1Time, not_after: Asn1Time) -> X509 {
        let rsa = Rsa::generate(2048).unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();

        let mut name_builder = X509NameBuilder::new().unwrap();
        name_builder
            .append_entry_by_text("CN", "Test Cert")
            .unwrap();
        let name = name_builder.build();

        let mut builder = X509Builder::new().unwrap();
        builder.set_version(2).unwrap();

        let serial = BigNum::from_u32(1).unwrap();
        builder
            .set_serial_number(&serial.to_asn1_integer().unwrap())
            .unwrap();

        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&pkey).unwrap();

        builder.set_not_before(&not_before).unwrap();
        builder.set_not_after(&not_after).unwrap();

        builder
            .append_extension(BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        builder
            .append_extension(KeyUsage::new().critical().key_cert_sign().build().unwrap())
            .unwrap();

        builder.sign(&pkey, MessageDigest::sha256()).unwrap();
        builder.build()
    }

    /// Create a test certificate valid from `days_ago` to `valid_for_days` from now
    fn create_valid_test_cert(days_ago: u32, valid_for_days: u32) -> X509 {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let not_before = Asn1Time::from_unix(now_secs - (days_ago as i64 * 86400)).unwrap();
        let not_after = Asn1Time::from_unix(now_secs + (valid_for_days as i64 * 86400)).unwrap();
        create_test_cert_with_times(not_before, not_after)
    }

    /// Create an expired test certificate
    fn create_expired_test_cert() -> X509 {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let not_before = Asn1Time::from_unix(now_secs - (20 * 86400)).unwrap(); // 20 days ago
        let not_after = Asn1Time::from_unix(now_secs - 86400).unwrap(); // 1 day ago
        create_test_cert_with_times(not_before, not_after)
    }

    /// Create a not-yet-valid test certificate
    fn create_future_test_cert() -> X509 {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let not_before = Asn1Time::from_unix(now_secs + 86400).unwrap(); // 1 day from now
        let not_after = Asn1Time::from_unix(now_secs + (10 * 86400)).unwrap(); // 10 days from now
        create_test_cert_with_times(not_before, not_after)
    }

    #[test]
    fn test_certificate_validity_current_time() {
        // Create a certificate valid from 10 days ago to 10 days in future
        let cert = create_valid_test_cert(10, 10);
        let now = Asn1Time::days_from_now(0).unwrap();

        // Should succeed
        assert!(check_certificate_validity(&cert, &now).is_ok());
    }

    #[test]
    fn test_certificate_expired() {
        // Create a certificate that expired yesterday
        let cert = create_expired_test_cert();
        let now = Asn1Time::days_from_now(0).unwrap();

        // Should fail with CertificateExpired
        let result = check_certificate_validity(&cert, &now);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                ServerError::Attestation(AttestationError::CertificateExpired { .. })
            ),
            "Expected CertificateExpired, got: {err:?}"
        );
    }

    #[test]
    fn test_certificate_not_yet_valid() {
        // Create a certificate that starts tomorrow
        let cert = create_future_test_cert();
        let now = Asn1Time::days_from_now(0).unwrap();

        // Should fail with CertificateNotYetValid
        let result = check_certificate_validity(&cert, &now);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                ServerError::Attestation(AttestationError::CertificateNotYetValid { .. })
            ),
            "Expected CertificateNotYetValid, got: {err:?}"
        );
    }

    #[test]
    fn test_verify_options_with_time() {
        let future_time = SystemTime::now() + Duration::from_secs(86400 * 30); // 30 days in future
        let options = VerifyOptions::new().with_time(future_time);

        assert!(options.current_time.is_some());
    }

    #[test]
    fn test_verify_options_default() {
        let options = VerifyOptions::default();
        assert!(options.current_time.is_none());
    }

    // Note: Full attestation verification tests require actual attestation documents
    // from a Nitro Enclave, which are not available in unit tests.
    // Integration tests should be run in a Nitro Enclave environment.
}
