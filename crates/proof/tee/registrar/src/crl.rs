//! On-demand CRL (Certificate Revocation List) checking for AWS Nitro
//! intermediate certificates.
//!
//! Extracts CRL distribution point URLs and serial numbers from DER-encoded
//! X.509 certificates, fetches the CRL from AWS S3, and checks whether any
//! certificate has been revoked.
//!
//! The check is **fail-open**: if a CRL cannot be fetched or parsed, the
//! registration proceeds with a warning. The on-chain expiry tracking
//! (Step 2) provides a backstop for expired certs.
//!
//! **TOCTOU note**: there is an inherent race between the CRL check and the
//! on-chain registration transaction. A certificate could be added to the
//! CRL after the check passes but before the registration lands. This is
//! acceptable because the on-chain expiry backstop and periodic re-checks
//! bound the exposure window.

use std::time::Duration;

use alloy_primitives::B256;
use base_proof_tee_nitro_verifier::compute_path_digests;
use tracing::{debug, warn};
use x509_parser::{
    certificate::X509Certificate,
    extensions::ParsedExtension,
    prelude::{FromDer, GeneralName},
    revocation_list::CertificateRevocationList,
};

/// Default HTTP timeout for CRL fetches from AWS S3.
pub const DEFAULT_CRL_FETCH_TIMEOUT_SECS: u64 = 30;

/// Maximum allowed CRL response body size (10 `MiB`).
///
/// Prevents unbounded memory allocation when fetching CRLs. AWS Nitro CRLs
/// are typically a few KB; this cap is generous to accommodate growth while
/// still protecting against resource exhaustion from malicious or corrupted
/// responses.
const MAX_CRL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// Allowed hostname suffix for CRL distribution point URLs.
///
/// All legitimate AWS Nitro CRL distribution points are hosted on S3
/// endpoints matching this pattern. Rejecting other hosts prevents SSRF
/// via attacker-controlled attestation documents.
const ALLOWED_CRL_HOST_SUFFIX: &str = ".amazonaws.com";

/// Required substring in the hostname of CRL distribution point URLs.
///
/// In addition to the `.amazonaws.com` suffix, all legitimate AWS Nitro CRL
/// URLs contain `nitro-enclave` in the hostname (e.g.
/// `aws-nitro-enclaves-crl.s3.amazonaws.com`). This provides a second layer
/// of allowlisting.
const ALLOWED_CRL_HOST_KEYWORD: &str = "nitro-enclave";

/// Information extracted from a single certificate needed for CRL checking.
#[derive(Debug, Clone)]
pub struct CertCrlInfo {
    /// Human-readable label for logging (e.g. "intermediate 1").
    pub label: String,
    /// Serial number of the certificate (big-endian, unsigned).
    pub serial_number: Vec<u8>,
    /// CRL distribution point URL, if present in the certificate.
    pub crl_url: Option<String>,
    /// Accumulated path digest for this certificate position (for on-chain
    /// `revokeCert` calls).
    pub path_digest: B256,
}

impl CertCrlInfo {
    /// Extracts CRL-relevant information from each certificate in a
    /// DER-encoded chain.
    ///
    /// The certificates must be in chain order: root → intermediates → leaf.
    /// Path digests are computed identically to the on-chain
    /// `NitroEnclaveVerifier` accumulation.
    ///
    /// # Errors
    ///
    /// Returns an error if any certificate cannot be parsed from DER.
    pub fn from_chain(certs_der: &[&[u8]]) -> Result<Vec<Self>, CrlError> {
        let mut infos = Vec::with_capacity(certs_der.len());

        for (i, der) in certs_der.iter().enumerate() {
            let (remaining, cert) = X509Certificate::from_der(der)
                .map_err(|e| CrlError::CertParse(format!("certificate {i}: {e}")))?;
            if !remaining.is_empty() {
                return Err(CrlError::CertParse(format!(
                    "certificate {i}: trailing DER data ({} bytes)",
                    remaining.len()
                )));
            }

            let serial_number = cert.tbs_certificate.serial.to_bytes_be();
            let crl_url = extract_crl_distribution_point(&cert);

            let label = if i == 0 {
                "root".to_string()
            } else if i == certs_der.len() - 1 {
                "leaf".to_string()
            } else {
                format!("intermediate {i}")
            };

            infos.push(Self { label, serial_number, crl_url, path_digest: B256::ZERO });
        }

        let path_digests = compute_path_digests(certs_der);
        for (info, path_digest) in infos.iter_mut().zip(path_digests) {
            info.path_digest = path_digest;
        }

        Ok(infos)
    }

    /// Returns an iterator over the intermediate certificates in a chain,
    /// skipping the root (index 0) and the leaf (last index).
    ///
    /// Roots manage their own trust and leaves are short-lived
    /// (~3 hours), so neither participates in the on-chain
    /// `_cacheNewCert` rewrite that the durable revocation sentinel
    /// guards against; the AWS CRL layer applies the same scope.
    /// Chains shorter than three certificates yield an empty iterator.
    pub fn intermediates(infos: &[Self]) -> impl Iterator<Item = &Self> {
        infos.iter().skip(1).take(infos.len().saturating_sub(2))
    }
}

/// Information about a revoked certificate.
#[derive(Debug, Clone)]
pub struct RevokedCertInfo {
    /// Label of the revoked certificate (e.g. "intermediate 1").
    pub label: String,
    /// Path digest for on-chain `revokeCert()`.
    pub path_digest: B256,
}

/// Extracts the first HTTP/HTTPS CRL distribution point URL from a
/// certificate's extensions.
///
/// Returns `None` if the certificate has no CRL distribution points
/// extension or if none of the distribution points contain a URI.
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

/// Returns `true` if the given URL's host is an allowed AWS CRL endpoint.
///
/// Validates that the hostname ends with [`.amazonaws.com`](ALLOWED_CRL_HOST_SUFFIX)
/// and contains [`nitro-enclave`](ALLOWED_CRL_HOST_KEYWORD). This prevents
/// SSRF attacks via attacker-controlled CRL distribution point URLs in
/// unverified attestation documents.
fn is_allowed_crl_host(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_lowercase())).is_some_and(
        |host| host.ends_with(ALLOWED_CRL_HOST_SUFFIX) && host.contains(ALLOWED_CRL_HOST_KEYWORD),
    )
}

/// Checks a pre-parsed certificate chain against CRLs fetched from
/// distribution points.
///
/// For each intermediate (root and leaf are skipped via
/// [`CertCrlInfo::intermediates`]) the function:
/// 1. Reads the CRL distribution point URL recorded during parsing
/// 2. Fetches the CRL
/// 3. Checks whether the certificate's serial number appears on the CRL
///
/// **Fail-open policy**: CRL fetch or parse failures are logged as warnings
/// but do not abort the check. Only confirmed revocations are reported.
///
/// # Arguments
///
/// * `cert_infos` - Pre-parsed cert chain info, typically produced once per
///   cycle by [`CertCrlInfo::from_chain`] and shared with the on-chain
///   revocation pre-check so the DER parse only happens once.
/// * `http_client` - HTTP client for fetching CRLs
pub async fn check_chain_against_crls(
    cert_infos: &[CertCrlInfo],
    http_client: &reqwest::Client,
) -> Vec<RevokedCertInfo> {
    let mut revoked = Vec::new();

    for info in CertCrlInfo::intermediates(cert_infos) {
        let Some(ref crl_url) = info.crl_url else {
            debug!(cert = %info.label, "no CRL distribution point, skipping");
            continue;
        };

        debug!(cert = %info.label, url = %crl_url, "fetching CRL");

        match fetch_and_check_crl(http_client, crl_url, &info.serial_number).await {
            Ok(true) => {
                warn!(
                    cert = %info.label,
                    url = %crl_url,
                    serial = %hex::encode(&info.serial_number),
                    path_digest = %info.path_digest,
                    "certificate found on CRL — REVOKED"
                );
                revoked.push(RevokedCertInfo {
                    label: info.label.clone(),
                    path_digest: info.path_digest,
                });
            }
            Ok(false) => {
                debug!(cert = %info.label, "certificate not on CRL");
            }
            Err(e) => {
                // Fail-open: log warning but continue.
                warn!(
                    cert = %info.label,
                    url = %crl_url,
                    error = %e,
                    "CRL check failed (fail-open, proceeding)"
                );
            }
        }
    }

    revoked
}

/// Fetches a CRL from the given URL and checks if the serial number is
/// present.
///
/// Returns `Ok(true)` if the serial number is on the CRL (revoked),
/// `Ok(false)` if not, and `Err` if the CRL could not be fetched or parsed.
///
/// The URL is validated against an allowlist before fetching to prevent SSRF.
/// Responses larger than [`MAX_CRL_RESPONSE_BYTES`] are rejected.
async fn fetch_and_check_crl(
    http_client: &reqwest::Client,
    crl_url: &str,
    serial_number: &[u8],
) -> Result<bool, CrlError> {
    // Reject URLs that don't match the AWS Nitro CRL host allowlist.
    if !is_allowed_crl_host(crl_url) {
        return Err(CrlError::Fetch(format!(
            "{crl_url}: host not in CRL allowlist (must be *{ALLOWED_CRL_HOST_SUFFIX} \
             containing '{ALLOWED_CRL_HOST_KEYWORD}')"
        )));
    }

    let response = http_client
        .get(crl_url)
        .send()
        .await
        .map_err(|e| CrlError::Fetch(format!("{crl_url}: {e}")))?;

    if !response.status().is_success() {
        return Err(CrlError::Fetch(format!("{crl_url}: HTTP {}", response.status())));
    }

    // Check Content-Length header if present to reject obviously oversized
    // responses before buffering the body.
    if let Some(content_length) = response.content_length()
        && content_length > MAX_CRL_RESPONSE_BYTES as u64
    {
        return Err(CrlError::Fetch(format!(
            "{crl_url}: response too large ({content_length} bytes, max {MAX_CRL_RESPONSE_BYTES})"
        )));
    }

    let crl_bytes = response
        .bytes()
        .await
        .map_err(|e| CrlError::Fetch(format!("{crl_url}: failed to read body: {e}")))?;

    if crl_bytes.len() > MAX_CRL_RESPONSE_BYTES {
        return Err(CrlError::Fetch(format!(
            "{crl_url}: response too large ({} bytes, max {MAX_CRL_RESPONSE_BYTES})",
            crl_bytes.len()
        )));
    }

    let (remaining, crl) = CertificateRevocationList::from_der(&crl_bytes)
        .map_err(|e| CrlError::Parse(format!("{crl_url}: {e}")))?;
    if !remaining.is_empty() {
        return Err(CrlError::Parse(format!(
            "{crl_url}: trailing DER data ({} bytes)",
            remaining.len()
        )));
    }

    // Check if the serial number appears in the revoked certificates list.
    // Both sides use `BigUint::to_bytes_be()` which normalises away ASN.1
    // leading-zero padding, so direct comparison is correct.
    for revoked_cert in crl.iter_revoked_certificates() {
        if revoked_cert.user_certificate.to_bytes_be() == serial_number {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Creates an HTTP client configured for CRL fetching.
///
/// Uses a timeout to prevent hanging on unresponsive S3 endpoints.
/// Redirects are disabled to prevent SSRF via open-redirect chains.
pub fn build_crl_http_client(timeout: Duration) -> Result<reqwest::Client, CrlError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CrlError::Config(format!("failed to build HTTP client: {e}")))
}

/// Errors specific to CRL checking.
#[derive(Debug, thiserror::Error)]
pub enum CrlError {
    /// Failed to parse a certificate from DER.
    #[error("certificate parse error: {0}")]
    CertParse(String),

    /// Failed to fetch a CRL from a distribution point.
    #[error("CRL fetch error: {0}")]
    Fetch(String),

    /// Failed to parse a CRL.
    #[error("CRL parse error: {0}")]
    Parse(String),

    /// Configuration error.
    #[error("CRL config error: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener},
        thread::{self, JoinHandle},
    };

    use rstest::{fixture, rstest};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::test_utils::{
        CertFixtures, INTER1_HEX, INTER2_HEX, INTER3_HEX, INVALID_DER_BYTES, LEAF_HEX, ROOT_HEX,
    };

    // ── Expected constants ──────────────────────────────────────────────

    /// Expected CRL distribution point URL for intermediate 1.
    const INTER1_EXPECTED_CRL_URL: &str = "http://aws-nitro-enclaves-crl.s3.amazonaws.com/crl/ab4960cc-7d63-42bd-9e9f-59338cb67f84.crl";

    /// Expected CRL distribution point URL for intermediate 2.
    const INTER2_EXPECTED_CRL_URL: &str = "http://crl-us-east-1-aws-nitro-enclaves.s3.us-east-1.amazonaws.com/crl/06d48f8e-2c08-4781-a645-b1de402aefb8.crl";

    /// Expected serial number (hex) for root CA (`BigUint::to_bytes_be()`).
    const ROOT_EXPECTED_SERIAL_HEX: &str = "f93175681b90afe11d46ccb4e4e7f856";

    /// Expected serial number (hex) for intermediate 1.
    const INTER1_EXPECTED_SERIAL_HEX: &str = "56bfc987fd05ac99c475061b1a65eedc";

    /// Expected serial number (hex) for intermediate 2 (`BigUint::to_bytes_be()`).
    const INTER2_EXPECTED_SERIAL_HEX: &str = "cb286a4a4a09207f8b0c14950dcd6861";

    /// Expected serial number (hex) for intermediate 3.
    const INTER3_EXPECTED_SERIAL_HEX: &str = "c8925d382506d820d93d2c704a7523c4ba2ddfaa";

    /// Expected serial number (hex) for the leaf/enclave certificate.
    const LEAF_EXPECTED_SERIAL_HEX: &str = "0193685e7fee7d8500000000674b3bd8";

    /// Number of certificates in the full Nitro attestation chain
    /// (root + 3 intermediates + leaf).
    const FULL_CHAIN_LEN: usize = 5;

    /// Minimal valid DER-encoded CRL with no revoked certificates.
    const EMPTY_CRL_DER: [u8; 49] = [
        0x30, 0x2f, 0x30, 0x1d, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03,
        0x03, 0x30, 0x00, 0x17, 0x0d, 0x32, 0x34, 0x30, 0x31, 0x30, 0x31, 0x30, 0x30, 0x30, 0x30,
        0x30, 0x30, 0x5a, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03,
        0x03, 0x02, 0x00, 0x00,
    ];

    const CRL_TEST_HOST: &str = "aws-nitro-enclave-test.amazonaws.com";

    // ── Fixtures ────────────────────────────────────────────────────────

    /// Decoded DER bytes and their slice references for the full 5-cert
    /// chain, ready for use in `CertCrlInfo::from_chain`.
    struct ChainFixture {
        owned: Vec<Vec<u8>>,
    }

    impl ChainFixture {
        fn refs(&self) -> Vec<&[u8]> {
            self.owned.iter().map(|c| c.as_slice()).collect()
        }
    }

    /// Full 5-cert chain (root → 3 intermediates → leaf) from real Nitro
    /// attestation.
    #[fixture]
    fn full_chain() -> ChainFixture {
        ChainFixture {
            owned: CertFixtures::decode_chain(&[
                ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX,
            ]),
        }
    }

    /// Single-cert chain containing only the root.
    #[fixture]
    fn root_only_chain() -> ChainFixture {
        ChainFixture { owned: CertFixtures::decode_chain(&[ROOT_HEX]) }
    }

    /// Two-cert chain containing root and leaf (no intermediates).
    #[fixture]
    fn root_and_leaf_chain() -> ChainFixture {
        ChainFixture { owned: CertFixtures::decode_chain(&[ROOT_HEX, LEAF_HEX]) }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Parses a single hex-encoded cert and extracts its CRL distribution
    /// point URL.
    fn crl_url_for_hex(cert_hex: &str) -> Option<String> {
        let der = CertFixtures::decode(cert_hex);
        let (remaining, cert) = X509Certificate::from_der(&der).unwrap();
        assert!(remaining.is_empty());
        extract_crl_distribution_point(&cert)
    }

    fn serve_crl_body_once(body: Vec<u8>) -> (reqwest::Client, String, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            assert!(bytes_read > 0);

            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let client = client_resolving_crl_host_to(addr);
        let url = format!("http://{CRL_TEST_HOST}/crl");

        (client, url, handle)
    }

    fn client_resolving_crl_host_to(addr: SocketAddr) -> reqwest::Client {
        reqwest::Client::builder().no_proxy().resolve(CRL_TEST_HOST, addr).build().unwrap()
    }

    // ── CRL distribution point extraction (parameterised) ───────────────

    #[rstest]
    #[case::intermediate_1(INTER1_HEX, Some(INTER1_EXPECTED_CRL_URL))]
    #[case::intermediate_2(INTER2_HEX, Some(INTER2_EXPECTED_CRL_URL))]
    #[case::intermediate_3_has_no_crl_dp(INTER3_HEX, None)]
    #[case::root_has_no_crl_dp(ROOT_HEX, None)]
    #[case::leaf_has_no_crl_dp(LEAF_HEX, None)]
    fn extracts_crl_distribution_point_url(#[case] cert_hex: &str, #[case] expected: Option<&str>) {
        let url = crl_url_for_hex(cert_hex);
        assert_eq!(url.as_deref(), expected);
    }

    // ── Serial number extraction (parameterised) ────────────────────────

    #[rstest]
    #[case::root(0, ROOT_EXPECTED_SERIAL_HEX)]
    #[case::intermediate_1(1, INTER1_EXPECTED_SERIAL_HEX)]
    #[case::intermediate_2(2, INTER2_EXPECTED_SERIAL_HEX)]
    #[case::intermediate_3(3, INTER3_EXPECTED_SERIAL_HEX)]
    #[case::leaf(4, LEAF_EXPECTED_SERIAL_HEX)]
    fn extracts_correct_serial_number(
        full_chain: ChainFixture,
        #[case] index: usize,
        #[case] expected_hex: &str,
    ) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();
        assert_eq!(hex::encode(&infos[index].serial_number), expected_hex);
    }

    // ── Path digest consistency ─────────────────────────────────────────

    #[rstest]
    fn path_digests_match_onchain_computation(full_chain: ChainFixture) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();

        // First digest = sha256(root_der).
        let root_hash = B256::from_slice(Sha256::digest(&full_chain.owned[0]).as_slice());
        assert_eq!(infos[0].path_digest, root_hash);

        // Second digest = sha256(root_hash || sha256(inter1_der)).
        let inter1_hash = B256::from_slice(Sha256::digest(&full_chain.owned[1]).as_slice());
        let mut hasher = Sha256::new();
        hasher.update(root_hash.as_slice());
        hasher.update(inter1_hash.as_slice());
        let expected = B256::from_slice(hasher.finalize().as_slice());
        assert_eq!(infos[1].path_digest, expected);
    }

    // ── CertCrlInfo::from_chain: full chain properties ────────────────────

    #[rstest]
    fn full_chain_has_correct_count(full_chain: ChainFixture) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();
        assert_eq!(infos.len(), FULL_CHAIN_LEN);
    }

    #[rstest]
    fn labels_are_correct(full_chain: ChainFixture) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();
        assert_eq!(infos[0].label, "root");
        assert_eq!(infos[1].label, "intermediate 1");
        assert_eq!(infos[2].label, "intermediate 2");
        assert_eq!(infos[3].label, "intermediate 3");
        assert_eq!(infos[4].label, "leaf");
    }

    #[rstest]
    fn intermediates_1_and_2_have_crl_urls(full_chain: ChainFixture) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();

        assert!(infos[0].crl_url.is_none(), "root should not have a CRL URL");
        assert_eq!(
            infos[1].crl_url.as_deref(),
            Some(INTER1_EXPECTED_CRL_URL),
            "intermediate 1 should have a CRL URL"
        );
        assert_eq!(
            infos[2].crl_url.as_deref(),
            Some(INTER2_EXPECTED_CRL_URL),
            "intermediate 2 should have a CRL URL"
        );
        assert!(
            infos[3].crl_url.is_none(),
            "intermediate 3 does not have a CRL distribution points extension"
        );
        assert!(infos[4].crl_url.is_none(), "leaf should not have a CRL URL");
    }

    // ── Edge cases: empty and invalid chains ────────────────────────────

    #[rstest]
    fn empty_chain_returns_empty_vec() {
        let result = CertCrlInfo::from_chain(&[]);
        assert!(result.unwrap().is_empty());
    }

    #[rstest]
    fn invalid_der_returns_cert_parse_error() {
        let result = CertCrlInfo::from_chain(&[&INVALID_DER_BYTES[..]]);
        let err = result.unwrap_err();
        assert!(matches!(&err, CrlError::CertParse(_)), "expected CertParse, got: {err}");
    }

    #[rstest]
    fn trailing_der_certificate_alias_returns_cert_parse_error(full_chain: ChainFixture) {
        let mut aliased = full_chain.owned;
        aliased[1].extend_from_slice(b"chain-4256-trailing-der");
        let refs: Vec<&[u8]> = aliased.iter().map(|c| c.as_slice()).collect();
        let err = CertCrlInfo::from_chain(&refs).unwrap_err();
        let msg = err.to_string();

        assert!(matches!(&err, CrlError::CertParse(_)), "expected CertParse, got: {msg}");
        assert!(msg.contains("certificate 1"), "expected cert index in error, got: {msg}");
        assert!(msg.contains("trailing DER data"), "expected trailing DER error, got: {msg}");
    }

    // ── Edge cases: single-cert and two-cert chains ─────────────────────

    #[rstest]
    fn single_cert_chain_labels_as_root_and_leaf(root_only_chain: ChainFixture) {
        let refs = root_only_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();
        // A single cert is both first (root) and last (leaf). The labelling
        // logic checks `i == 0` first, so it should be labelled "root".
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].label, "root");
    }

    #[rstest]
    fn two_cert_chain_labels_root_and_leaf(root_and_leaf_chain: ChainFixture) {
        let refs = root_and_leaf_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].label, "root");
        assert_eq!(infos[1].label, "leaf");
        // No intermediates → no CRL URLs expected.
        assert!(infos[0].crl_url.is_none());
        assert!(infos[1].crl_url.is_none());
    }

    // ── build_crl_http_client ───────────────────────────────────────────

    #[rstest]
    fn build_crl_http_client_succeeds_with_valid_timeout() {
        let client = build_crl_http_client(Duration::from_secs(DEFAULT_CRL_FETCH_TIMEOUT_SECS));
        assert!(client.is_ok());
    }

    #[rstest]
    fn build_crl_http_client_succeeds_with_zero_timeout() {
        // Zero timeout is technically valid for reqwest — it means no
        // timeout. The builder should still succeed.
        let client = build_crl_http_client(Duration::ZERO);
        assert!(client.is_ok());
    }

    // ── check_chain_against_crls: empty / no-intermediate chains ────────

    #[tokio::test]
    #[rstest]
    async fn check_chain_against_crls_clean_for_empty_chain() {
        let client = build_crl_http_client(Duration::from_secs(5)).unwrap();
        let result = check_chain_against_crls(&[], &client).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    #[rstest]
    async fn check_chain_against_crls_skips_root_and_leaf(root_and_leaf_chain: ChainFixture) {
        // With only root + leaf and no intermediates, no CRL fetches should
        // be attempted, so the result is Clean even without network access.
        let client = build_crl_http_client(Duration::from_secs(1)).unwrap();
        let refs = root_and_leaf_chain.refs();
        let cert_infos = CertCrlInfo::from_chain(&refs).unwrap();
        let result = check_chain_against_crls(&cert_infos, &client).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn fetch_and_check_crl_accepts_exact_der_response() {
        let (client, url, handle) = serve_crl_body_once(EMPTY_CRL_DER.to_vec());
        let revoked = fetch_and_check_crl(&client, &url, &[]).await.unwrap();
        handle.join().unwrap();

        assert!(!revoked);
    }

    #[tokio::test]
    async fn fetch_and_check_crl_rejects_trailing_der_response() {
        let mut aliased = EMPTY_CRL_DER.to_vec();
        aliased.extend_from_slice(b"chain-4256-trailing-crl");
        let (client, url, handle) = serve_crl_body_once(aliased);
        let err = fetch_and_check_crl(&client, &url, &[]).await.unwrap_err();
        handle.join().unwrap();
        let msg = err.to_string();

        assert!(matches!(&err, CrlError::Parse(_)), "expected Parse, got: {msg}");
        assert!(msg.contains("trailing DER data"), "expected trailing DER error, got: {msg}");
    }

    // ── fetch_and_check_crl: serial comparison logic ────────────────────

    #[rstest]
    fn serial_bytes_are_correctly_decoded() {
        // Verify that the serial hex constants round-trip through the
        // full chain extraction, confirming the `to_bytes_be()` stripping
        // behaviour is consistent.
        let expected_serials = [
            ROOT_EXPECTED_SERIAL_HEX,
            INTER1_EXPECTED_SERIAL_HEX,
            INTER2_EXPECTED_SERIAL_HEX,
            INTER3_EXPECTED_SERIAL_HEX,
            LEAF_EXPECTED_SERIAL_HEX,
        ];
        for serial_hex in &expected_serials {
            let bytes = hex::decode(serial_hex).unwrap();
            assert!(!bytes.is_empty(), "serial {serial_hex} decoded to empty bytes");
            // Round-trip: decode → re-encode should be identical.
            assert_eq!(&hex::encode(&bytes), *serial_hex);
        }
    }

    // ── is_allowed_crl_host tests ───────────────────────────────────────

    #[rstest]
    #[case::inter1_url(INTER1_EXPECTED_CRL_URL, true)]
    #[case::inter2_url(INTER2_EXPECTED_CRL_URL, true)]
    #[case::evil_host("http://evil.com/crl/something.crl", false)]
    #[case::partial_match_no_keyword("http://s3.amazonaws.com/crl.crl", false)]
    #[case::no_suffix("http://nitro-enclaves-crl.example.com/crl.crl", false)]
    fn crl_host_allowlist_check(#[case] url: &str, #[case] expected: bool) {
        assert_eq!(
            is_allowed_crl_host(url),
            expected,
            "is_allowed_crl_host({url}) should be {expected}"
        );
    }
}
