//! AWS Nitro Enclave CA root certificates.
//!
//! This module handles loading and validating the AWS CA roots used
//! to verify Nitro Enclave attestation documents.

use base64::Engine;
use openssl::x509::X509;
use sha2::{Digest, Sha256};
use std::io::Read;
use x509_cert::Certificate;
use x509_cert::der::Decode;

use crate::error::{AttestationError, ServerError};

/// Default CA roots for AWS Nitro Enclaves (base64-encoded zip).
///
/// You can download them from:
/// <https://docs.aws.amazon.com/enclaves/latest/user/verify-root.html>
pub const DEFAULT_CA_ROOTS: &str = "UEsDBBQAAAAIALkYV1GVtvolRwIAAAkDAAAIABwAcm9vdC5wZW1VVAkAA10ekl9dHpJfdXgLAAEESHEtDwQUAAAAZZJLk6JQDIX3/IrZW10Igo2LWdwXiBoE5HXZCSq0iNgKfYVfP9guJ8tTqS85Ofn4GAszy3b+EOYHtmkTFLCX+CGBbRMWEILSfYGEjVFh+8itnoe4yKq1XC7DDNptcJ2YXJCC2+smtYfzlCEBYhewjQSospASMlwCiSJ40gE5uHAijBrAldny5PaTnRkAan77iBDUiw4B+A9heZxKkedRilflYQZdVl+meW20aayfM8tU0wTEsswdCKonUFuDAPotRUo8ag59axIE3ls84xV4D0FG6gi1mFhF4cBcQNP35GIcGCvlsV504ImXnVffRqLjxpECT2tA6Xt1AFabs7zXu33i91mvXLLaefAkveQDVgEjC/ff1g60BSqYJeFdhzFCX0i1EXYFibZdTWA57Jf0q26/vZ+Ka3BbDVlz2chy2qv8wnYK9vVgVz1OWSZpBjFi3PTtp6li8Xlk7X7vTprSUrNr+FgspofpKlGNIHe9hDA3nWGE7WPgcsEaEqdMKo2LzhtPBHkoL9YOgTEgKkZ//jRA3lLGKBRIMCwP6PCyuPQ0ZhZeWJFYoYfKlPzJMRZ6Ns9vM7feX087nQta/ALcN8CjqLCsV4yEvL2Pd6JIrRBYnEjgkfOpn/hNXi+S7qjxq4hrZxUhTTuhqavH6vbGG7HYchL5e3b82RjdVkn4vdOfLbixdD8BGSFfhv6IcbYS63Vy2M3xrfXMLs2Cz1kjF7hUvsPnRb46d0UNtwY/iftcuJtsMnckW2yGmcz/Sr+fzRz637f/A1BLAQIeAxQAAAAIALkYV1GVtvolRwIAAAkDAAAIABgAAAAAAAEAAACkgQAAAAByb290LnBlbVVUBQADXR6SX3V4CwABBEhxLQ8EFAAAAFBLBQYAAAAAAQABAE4AAACJAgAAAAA=";

/// SHA256 checksum of the decoded CA roots zip.
pub const DEFAULT_CA_ROOTS_SHA256: &str =
    "8cf60e2b2efca96c6a9e71e851d00c1b6991cc09eadbe64a6a1d1b1eb9faff7c";

/// AWS Nitro Enclave CA root certificate.
#[derive(Debug, Clone)]
pub struct AwsCaRoot {
    /// The PEM-encoded certificate data.
    pub pem_data: Vec<u8>,
    /// The parsed X.509 certificate (using x509-cert crate).
    pub certificate: Certificate,
    /// The parsed X.509 certificate (using OpenSSL) for chain verification.
    pub openssl_cert: X509,
}

impl AwsCaRoot {
    /// Load the default AWS CA roots.
    ///
    /// This decodes the embedded base64 zip, validates the checksum,
    /// and extracts the PEM certificate.
    pub fn load_default() -> Result<Self, ServerError> {
        // Decode base64
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(DEFAULT_CA_ROOTS)
            .map_err(|e| AttestationError::Base64Decode(e.to_string()))?;

        // Validate checksum
        let mut hasher = Sha256::new();
        hasher.update(&decoded);
        let hash = hasher.finalize();
        let hash_hex = hex::encode(hash);

        if hash_hex != DEFAULT_CA_ROOTS_SHA256 {
            return Err(AttestationError::ChecksumMismatch {
                expected: DEFAULT_CA_ROOTS_SHA256.to_string(),
                actual: hash_hex,
            }
            .into());
        }

        // Extract PEM from zip
        let cursor = std::io::Cursor::new(&decoded);
        let mut archive =
            zip::ZipArchive::new(cursor).map_err(|e| AttestationError::ZipRead(e.to_string()))?;

        let mut pem_data = Vec::new();
        {
            let mut file = archive
                .by_index(0)
                .map_err(|e| AttestationError::ZipRead(e.to_string()))?;
            file.read_to_end(&mut pem_data)
                .map_err(|e| AttestationError::ZipRead(e.to_string()))?;
        }

        // Parse the PEM certificate using x509-cert
        let certificate = Self::parse_pem_certificate(&pem_data)?;

        // Parse the PEM certificate using OpenSSL for chain verification
        let openssl_cert = X509::from_pem(&pem_data)
            .map_err(|e| AttestationError::PemParse(format!("OpenSSL PEM parse error: {e}")))?;

        Ok(Self {
            pem_data,
            certificate,
            openssl_cert,
        })
    }

    /// Parse a PEM-encoded certificate.
    fn parse_pem_certificate(pem_data: &[u8]) -> Result<Certificate, ServerError> {
        // Find the certificate data between BEGIN and END markers
        let pem_str =
            std::str::from_utf8(pem_data).map_err(|e| AttestationError::PemParse(e.to_string()))?;

        let begin_marker = "-----BEGIN CERTIFICATE-----";
        let end_marker = "-----END CERTIFICATE-----";

        let start = pem_str
            .find(begin_marker)
            .ok_or_else(|| AttestationError::PemParse("missing BEGIN marker".to_string()))?;
        let end = pem_str
            .find(end_marker)
            .ok_or_else(|| AttestationError::PemParse("missing END marker".to_string()))?;

        let b64_data = &pem_str[start + begin_marker.len()..end];
        let b64_clean: String = b64_data.chars().filter(|c| !c.is_whitespace()).collect();

        let der_data = base64::engine::general_purpose::STANDARD
            .decode(&b64_clean)
            .map_err(|e| AttestationError::PemParse(e.to_string()))?;

        Certificate::from_der(&der_data)
            .map_err(|e| AttestationError::PemParse(e.to_string()))
            .map_err(Into::into)
    }
}

/// Lazily initialized AWS CA root.
static AWS_CA_ROOT: std::sync::OnceLock<Result<AwsCaRoot, String>> = std::sync::OnceLock::new();

/// Get the default AWS CA root certificate.
///
/// This is lazily initialized on first call.
pub fn get_default_ca_root() -> Result<&'static AwsCaRoot, ServerError> {
    let result = AWS_CA_ROOT.get_or_init(|| AwsCaRoot::load_default().map_err(|e| e.to_string()));

    match result {
        Ok(root) => Ok(root),
        Err(e) => Err(AttestationError::PemParse(e.clone()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_ca_roots() {
        let root = AwsCaRoot::load_default().expect("failed to load CA roots");
        assert!(!root.pem_data.is_empty());
    }

    #[test]
    fn test_checksum_validation() {
        // Decode and verify checksum
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(DEFAULT_CA_ROOTS)
            .expect("failed to decode");

        let mut hasher = Sha256::new();
        hasher.update(&decoded);
        let hash = hasher.finalize();
        let hash_hex = hex::encode(hash);

        assert_eq!(hash_hex, DEFAULT_CA_ROOTS_SHA256);
    }

    #[test]
    fn test_get_default_ca_root() {
        let root = get_default_ca_root().expect("failed to get CA root");
        assert!(!root.pem_data.is_empty());
    }
}
