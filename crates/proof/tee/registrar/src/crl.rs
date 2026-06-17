//! CRL (Certificate Revocation List) checking for AWS Nitro intermediate
//! certificates.
//!
//! Fetch and parse failures are fail-open; onchain expiry tracking and
//! periodic re-checks bound the remaining exposure window.

use alloy_primitives::B256;
use base_proof_tee_nitro_verifier::compute_path_digests;
use tracing::{debug, warn};
use x509_parser::{
    certificate::X509Certificate,
    extensions::ParsedExtension,
    prelude::{FromDer, GeneralName},
    revocation_list::CertificateRevocationList,
};

const MAX_CRL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const ALLOWED_CRL_HOST_SUFFIX: &str = ".amazonaws.com";
const ALLOWED_CRL_HOST_KEYWORD: &str = "nitro-enclave";

/// Information extracted from a single certificate needed for CRL checking.
#[derive(Debug)]
pub struct CertCrlInfo {
    /// Position of the certificate in the chain.
    pub index: usize,
    /// Serial number of the certificate (big-endian, unsigned).
    pub serial_number: Vec<u8>,
    /// CRL distribution point URL, if present in the certificate.
    pub crl_url: Option<String>,
    /// Accumulated path digest for this certificate position (for onchain
    /// `revokeCert` calls).
    pub path_digest: B256,
}

impl CertCrlInfo {
    /// Extracts CRL-relevant information from a DER-encoded chain's intermediate certificates.
    ///
    /// The certificates must be in chain order: root → intermediates → leaf.
    /// Path digests are computed identically to the onchain
    /// `NitroEnclaveVerifier` accumulation.
    ///
    /// # Errors
    ///
    /// Returns an error if any intermediate certificate cannot be parsed from DER.
    pub fn from_chain(certs_der: &[&[u8]]) -> Result<Vec<Self>, CrlError> {
        let mut infos = Vec::with_capacity(certs_der.len().saturating_sub(2));
        let path_digests = compute_path_digests(certs_der);

        for (index, (der, path_digest)) in certs_der
            .iter()
            .zip(path_digests)
            .enumerate()
            .skip(1)
            .take(certs_der.len().saturating_sub(2))
        {
            let (remaining, cert) = X509Certificate::from_der(der).map_err(|e| {
                CrlError(format!("certificate parse error: certificate {index}: {e}"))
            })?;
            if !remaining.is_empty() {
                return Err(CrlError(format!(
                    "certificate parse error: certificate {index}: trailing DER data ({} bytes)",
                    remaining.len()
                )));
            }

            let serial_number = cert.tbs_certificate.serial.to_bytes_be();
            let crl_url = extract_crl_distribution_point(&cert);

            infos.push(Self { index, serial_number, crl_url, path_digest });
        }

        Ok(infos)
    }
}

fn extract_crl_distribution_point(cert: &X509Certificate<'_>) -> Option<String> {
    for ext in cert.extensions() {
        let ParsedExtension::CRLDistributionPoints(cdp) = ext.parsed_extension() else {
            continue;
        };
        for dp in cdp.iter() {
            let Some(name) = &dp.distribution_point else { continue };
            let x509_parser::extensions::DistributionPointName::FullName(names) = name else {
                continue;
            };
            for gn in names {
                let GeneralName::URI(uri) = gn else { continue };
                if uri.starts_with("http://") || uri.starts_with("https://") {
                    return Some(uri.to_string());
                }
            }
        }
    }
    None
}

fn is_allowed_crl_host(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.domain().is_some_and(|host| {
            host.ends_with(ALLOWED_CRL_HOST_SUFFIX) && host.contains(ALLOWED_CRL_HOST_KEYWORD)
        })
    })
}

/// Checks intermediate certificates against their CRL distribution points.
///
/// **Fail-open policy**: CRL fetch or parse failures are logged as warnings
/// but do not abort the check. Only confirmed revocations are reported.
///
/// # Arguments
///
/// * `cert_infos` - Pre-parsed cert chain info, typically produced once per
///   cycle by [`CertCrlInfo::from_chain`] and shared with the onchain
///   revocation pre-check so the DER parse only happens once.
/// * `http_client` - HTTP client for fetching CRLs.
pub async fn check_chain_against_crls<'a>(
    cert_infos: &'a [CertCrlInfo],
    http_client: &reqwest::Client,
) -> Vec<&'a CertCrlInfo> {
    let mut revoked = Vec::new();

    for info in cert_infos {
        let Some(ref crl_url) = info.crl_url else {
            debug!(cert_index = info.index, "no CRL distribution point, skipping");
            continue;
        };

        debug!(cert_index = info.index, url = %crl_url, "fetching CRL");

        match fetch_and_check_crl(http_client, crl_url, &info.serial_number).await {
            Ok(true) => {
                warn!(
                    cert_index = info.index,
                    url = %crl_url,
                    serial = %hex::encode(&info.serial_number),
                    path_digest = %info.path_digest,
                    "certificate found on CRL — REVOKED"
                );
                revoked.push(info);
            }
            Ok(false) => {
                debug!(cert_index = info.index, "certificate not on CRL");
            }
            Err(e) => {
                warn!(
                    cert_index = info.index,
                    url = %crl_url,
                    error = %e,
                    "CRL check failed (fail-open, proceeding)"
                );
            }
        }
    }

    revoked
}

async fn fetch_and_check_crl(
    http_client: &reqwest::Client,
    crl_url: &str,
    serial_number: &[u8],
) -> Result<bool, CrlError> {
    if !is_allowed_crl_host(crl_url) {
        return Err(CrlError(format!(
            "CRL fetch error: {crl_url}: host not in CRL allowlist (must be *{ALLOWED_CRL_HOST_SUFFIX} \
             containing '{ALLOWED_CRL_HOST_KEYWORD}')"
        )));
    }

    let response = http_client
        .get(crl_url)
        .send()
        .await
        .map_err(|e| CrlError(format!("CRL fetch error: {crl_url}: {e}")))?;

    if !response.status().is_success() {
        return Err(CrlError(format!("CRL fetch error: {crl_url}: HTTP {}", response.status())));
    }

    if let Some(content_length) = response.content_length()
        && content_length > MAX_CRL_RESPONSE_BYTES as u64
    {
        return Err(CrlError(format!(
            "CRL fetch error: {crl_url}: response too large ({content_length} bytes, max {MAX_CRL_RESPONSE_BYTES})"
        )));
    }

    let crl_bytes = response
        .bytes()
        .await
        .map_err(|e| CrlError(format!("CRL fetch error: {crl_url}: failed to read body: {e}")))?;

    if crl_bytes.len() > MAX_CRL_RESPONSE_BYTES {
        return Err(CrlError(format!(
            "CRL fetch error: {crl_url}: response too large ({} bytes, max {MAX_CRL_RESPONSE_BYTES})",
            crl_bytes.len()
        )));
    }

    crl_contains_serial(crl_url, &crl_bytes, serial_number)
}

fn crl_contains_serial(
    crl_url: &str,
    crl_bytes: &[u8],
    serial_number: &[u8],
) -> Result<bool, CrlError> {
    let (remaining, crl) = CertificateRevocationList::from_der(crl_bytes)
        .map_err(|e| CrlError(format!("CRL parse error: {crl_url}: {e}")))?;
    if !remaining.is_empty() {
        return Err(CrlError(format!(
            "CRL parse error: {crl_url}: trailing DER data ({} bytes)",
            remaining.len()
        )));
    }

    // `to_bytes_be()` normalizes away ASN.1 leading-zero padding.
    Ok(crl.iter_revoked_certificates().any(|revoked_cert| {
        revoked_cert.user_certificate.to_bytes_be().as_slice() == serial_number
    }))
}

/// Error specific to CRL checking.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CrlError(
    /// Error detail.
    pub String,
);

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;
    use crate::test_utils::{
        CertFixtures, INTER1_HEX, INTER2_HEX, INTER3_HEX, INVALID_DER_BYTES, LEAF_HEX, ROOT_HEX,
    };

    const INTER1_EXPECTED_CRL_URL: &str = "http://aws-nitro-enclaves-crl.s3.amazonaws.com/crl/ab4960cc-7d63-42bd-9e9f-59338cb67f84.crl";

    const INTER2_EXPECTED_CRL_URL: &str = "http://crl-us-east-1-aws-nitro-enclaves.s3.us-east-1.amazonaws.com/crl/06d48f8e-2c08-4781-a645-b1de402aefb8.crl";

    const EMPTY_CRL_DER: [u8; 49] = hex!(
        "302f301d300a06082a8648ce3d0403033000170d3234303130313030303030305a300a06082a8648ce3d04030303020000"
    );

    fn full_chain() -> Vec<Vec<u8>> {
        CertFixtures::decode_chain(&[ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX])
    }

    #[test]
    fn extracts_crl_distribution_point_url() {
        for (cert_hex, expected) in [
            (INTER1_HEX, Some(INTER1_EXPECTED_CRL_URL)),
            (INTER2_HEX, Some(INTER2_EXPECTED_CRL_URL)),
            (INTER3_HEX, None),
            (ROOT_HEX, None),
            (LEAF_HEX, None),
        ] {
            let der = CertFixtures::decode(cert_hex);
            let (remaining, cert) = X509Certificate::from_der(&der).unwrap();
            assert!(remaining.is_empty());
            let url = extract_crl_distribution_point(&cert);
            assert_eq!(url.as_deref(), expected);
        }
    }

    #[test]
    fn extracts_intermediate_cert_info_from_chain() {
        let full_chain = full_chain();
        let refs: Vec<&[u8]> = full_chain.iter().map(Vec::as_slice).collect();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();

        assert_eq!(infos.iter().map(|info| info.index).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(
            infos.iter().map(|info| hex::encode(&info.serial_number)).collect::<Vec<_>>(),
            vec![
                "56bfc987fd05ac99c475061b1a65eedc",
                "cb286a4a4a09207f8b0c14950dcd6861",
                "c8925d382506d820d93d2c704a7523c4ba2ddfaa",
            ]
        );
    }

    #[test]
    fn invalid_intermediate_der_returns_cert_parse_error() {
        let root = CertFixtures::decode(ROOT_HEX);
        let leaf = CertFixtures::decode(LEAF_HEX);
        let result = CertCrlInfo::from_chain(&[&root, &INVALID_DER_BYTES[..], &leaf]);
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("certificate parse error"),
            "expected certificate parse error, got: {err}"
        );
    }

    #[test]
    fn trailing_der_certificate_alias_returns_cert_parse_error() {
        let mut aliased = full_chain();
        aliased[1].extend_from_slice(b"chain-4256-trailing-der");
        let refs: Vec<&[u8]> = aliased.iter().map(Vec::as_slice).collect();
        let err = CertCrlInfo::from_chain(&refs).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("trailing DER data"), "expected trailing DER error, got: {msg}");
    }

    #[test]
    fn crl_parse_rejects_trailing_der() {
        let mut aliased = EMPTY_CRL_DER.to_vec();
        aliased.extend_from_slice(b"chain-4256-trailing-crl");
        let err = crl_contains_serial("test.crl", &aliased, &[]).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("trailing DER data"), "expected trailing DER error, got: {msg}");
    }

    #[test]
    fn crl_host_allowlist_check() {
        for (url, expected) in [
            (INTER1_EXPECTED_CRL_URL, true),
            (INTER2_EXPECTED_CRL_URL, true),
            ("http://AWS-NITRO-ENCLAVES-CRL.S3.AMAZONAWS.COM/crl/test.crl", true),
            ("http://evil.com/crl/something.crl", false),
            ("http://s3.amazonaws.com/crl.crl", false),
            ("http://nitro-enclaves-crl.example.com/crl.crl", false),
        ] {
            assert_eq!(
                is_allowed_crl_host(url),
                expected,
                "is_allowed_crl_host({url}) should be {expected}"
            );
        }
    }
}
