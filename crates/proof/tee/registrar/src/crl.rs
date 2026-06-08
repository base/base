//! CRL (Certificate Revocation List) checking for AWS Nitro intermediate
//! certificates.
//!
//! Fetch and parse failures are fail-open; onchain expiry tracking and
//! periodic re-checks bound the remaining exposure window.

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

/// Default timeout for CRL fetches.
pub const DEFAULT_CRL_FETCH_TIMEOUT_SECS: u64 = 30;

const MAX_CRL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const ALLOWED_CRL_HOST_SUFFIX: &str = ".amazonaws.com";
const ALLOWED_CRL_HOST_KEYWORD: &str = "nitro-enclave";

/// Information extracted from a single certificate needed for CRL checking.
#[derive(Debug, Clone)]
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
    /// Extracts CRL-relevant information from a DER-encoded chain.
    ///
    /// The certificates must be in chain order: root → intermediates → leaf.
    /// Path digests are computed identically to the onchain
    /// `NitroEnclaveVerifier` accumulation.
    ///
    /// # Errors
    ///
    /// Returns an error if any certificate cannot be parsed from DER.
    pub fn from_chain(certs_der: &[&[u8]]) -> Result<Vec<Self>, CrlError> {
        let mut infos = Vec::with_capacity(certs_der.len());
        let path_digests = compute_path_digests(certs_der);

        for (index, (der, path_digest)) in certs_der.iter().zip(path_digests).enumerate() {
            let (remaining, cert) = X509Certificate::from_der(der)
                .map_err(|e| CrlError::CertParse(format!("certificate {index}: {e}")))?;
            if !remaining.is_empty() {
                return Err(CrlError::CertParse(format!(
                    "certificate {index}: trailing DER data ({} bytes)",
                    remaining.len()
                )));
            }

            let serial_number = cert.tbs_certificate.serial.to_bytes_be();
            let crl_url = extract_crl_distribution_point(&cert);

            infos.push(Self { index, serial_number, crl_url, path_digest });
        }

        Ok(infos)
    }

    /// Returns intermediate certificates, skipping the root and leaf.
    ///
    /// Roots manage their own trust and leaves are short-lived
    /// (~3 hours), so neither participates in the onchain
    /// `_cacheNewCert` rewrite that the durable revocation sentinel
    /// guards against; the AWS CRL layer applies the same scope.
    /// Chains shorter than three certificates yield an empty iterator.
    pub fn intermediates(infos: &[Self]) -> impl Iterator<Item = &Self> {
        infos.iter().skip(1).take(infos.len().saturating_sub(2))
    }

    /// Returns the label used when logging this intermediate certificate.
    pub fn intermediate_label(&self) -> String {
        intermediate_cert_label(self.index)
    }
}

/// Information about a revoked certificate.
#[derive(Debug, Clone)]
pub struct RevokedCertInfo {
    /// Position of the revoked certificate in the chain.
    pub index: usize,
    /// Path digest for onchain `revokeCert()`.
    pub path_digest: B256,
}

impl RevokedCertInfo {
    /// Returns the label used when logging this revoked intermediate certificate.
    pub fn intermediate_label(&self) -> String {
        intermediate_cert_label(self.index)
    }
}

fn intermediate_cert_label(index: usize) -> String {
    format!("intermediate {index}")
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
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_lowercase())).is_some_and(
        |host| host.ends_with(ALLOWED_CRL_HOST_SUFFIX) && host.contains(ALLOWED_CRL_HOST_KEYWORD),
    )
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
pub async fn check_chain_against_crls(
    cert_infos: &[CertCrlInfo],
    http_client: &reqwest::Client,
) -> Vec<RevokedCertInfo> {
    let mut revoked = Vec::new();

    for info in CertCrlInfo::intermediates(cert_infos) {
        let cert = info.intermediate_label();
        let Some(ref crl_url) = info.crl_url else {
            debug!(cert = %cert, "no CRL distribution point, skipping");
            continue;
        };

        debug!(cert = %cert, url = %crl_url, "fetching CRL");

        match fetch_and_check_crl(http_client, crl_url, &info.serial_number).await {
            Ok(true) => {
                warn!(
                    cert = %cert,
                    url = %crl_url,
                    serial = %hex::encode(&info.serial_number),
                    path_digest = %info.path_digest,
                    "certificate found on CRL — REVOKED"
                );
                revoked.push(RevokedCertInfo { index: info.index, path_digest: info.path_digest });
            }
            Ok(false) => {
                debug!(cert = %cert, "certificate not on CRL");
            }
            Err(e) => {
                warn!(
                    cert = %cert,
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

    // `to_bytes_be()` normalizes away ASN.1 leading-zero padding.
    for revoked_cert in crl.iter_revoked_certificates() {
        if revoked_cert.user_certificate.to_bytes_be() == serial_number {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Builds a CRL HTTP client with redirects disabled.
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

    use alloy_primitives::{B256, b256};
    use rstest::{fixture, rstest};

    use super::*;
    use crate::test_utils::{
        CertFixtures, INTER1_HEX, INTER2_HEX, INTER3_HEX, INVALID_DER_BYTES, LEAF_HEX, ROOT_HEX,
    };

    const INTER1_EXPECTED_CRL_URL: &str = "http://aws-nitro-enclaves-crl.s3.amazonaws.com/crl/ab4960cc-7d63-42bd-9e9f-59338cb67f84.crl";

    const INTER2_EXPECTED_CRL_URL: &str = "http://crl-us-east-1-aws-nitro-enclaves.s3.us-east-1.amazonaws.com/crl/06d48f8e-2c08-4781-a645-b1de402aefb8.crl";

    const ROOT_EXPECTED_SERIAL_HEX: &str = "f93175681b90afe11d46ccb4e4e7f856";
    const INTER1_EXPECTED_SERIAL_HEX: &str = "56bfc987fd05ac99c475061b1a65eedc";
    const INTER2_EXPECTED_SERIAL_HEX: &str = "cb286a4a4a09207f8b0c14950dcd6861";
    const INTER3_EXPECTED_SERIAL_HEX: &str = "c8925d382506d820d93d2c704a7523c4ba2ddfaa";
    const LEAF_EXPECTED_SERIAL_HEX: &str = "0193685e7fee7d8500000000674b3bd8";
    const EXPECTED_PATH_DIGESTS: [B256; 5] = [
        b256!("641a0321a3e244efe456463195d606317ed7cdcc3c1756e09893f3c68f79bb5b"),
        b256!("aa413b647367f37da57079d2ae215fa2b14cb42ec0c4e4275f56dd3caff95b36"),
        b256!("bc023b9f717f6a435ab56642c5b5784179fb4d39166d36641d47887d3011c125"),
        b256!("f8cffb2fa4503ee3753a54d06d3dcbf96f4ea1db505cccc5c14f784f5234604a"),
        b256!("140ad974a8d3c771bf24a12fdbfff85a7191ba9a9a703869948aebedc16dd3ad"),
    ];
    const EMPTY_CRL_DER: [u8; 49] = [
        0x30, 0x2f, 0x30, 0x1d, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03,
        0x03, 0x30, 0x00, 0x17, 0x0d, 0x32, 0x34, 0x30, 0x31, 0x30, 0x31, 0x30, 0x30, 0x30, 0x30,
        0x30, 0x30, 0x5a, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03,
        0x03, 0x02, 0x00, 0x00,
    ];

    const CRL_TEST_HOST: &str = "aws-nitro-enclave-test.amazonaws.com";

    struct ChainFixture {
        owned: Vec<Vec<u8>>,
    }

    impl ChainFixture {
        fn refs(&self) -> Vec<&[u8]> {
            self.owned.iter().map(|c| c.as_slice()).collect()
        }
    }

    #[fixture]
    fn full_chain() -> ChainFixture {
        ChainFixture {
            owned: CertFixtures::decode_chain(&[
                ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX,
            ]),
        }
    }

    #[fixture]
    fn root_and_leaf_chain() -> ChainFixture {
        ChainFixture { owned: CertFixtures::decode_chain(&[ROOT_HEX, LEAF_HEX]) }
    }

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

    #[rstest]
    fn path_digests_match_onchain_computation(full_chain: ChainFixture) {
        let refs = full_chain.refs();
        let infos = CertCrlInfo::from_chain(&refs).unwrap();

        assert_eq!(infos.len(), EXPECTED_PATH_DIGESTS.len());
        for (info, expected_digest) in infos.iter().zip(EXPECTED_PATH_DIGESTS) {
            assert_eq!(info.path_digest, expected_digest);
        }
    }

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
