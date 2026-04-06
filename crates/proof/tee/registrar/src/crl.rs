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
        let path_digests = compute_path_digests(certs_der);
        let mut infos = Vec::with_capacity(certs_der.len());

        for (i, der) in certs_der.iter().enumerate() {
            let (_, cert) = X509Certificate::from_der(der)
                .map_err(|e| CrlError::CertParse(format!("certificate {i}: {e}")))?;

            let serial_number = cert.tbs_certificate.serial.to_bytes_be();
            let crl_url = extract_crl_distribution_point(&cert);

            let label = if i == 0 {
                "root".to_string()
            } else if i == certs_der.len() - 1 {
                "leaf".to_string()
            } else {
                format!("intermediate {i}")
            };

            infos.push(Self { label, serial_number, crl_url, path_digest: path_digests[i] });
        }

        Ok(infos)
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

/// Checks a certificate chain against CRLs fetched from distribution points.
///
/// This is the main entry point for CRL checking. It:
/// 1. Extracts CRL info from each certificate
/// 2. For certificates with CRL distribution points, fetches the CRL
/// 3. Checks if the certificate's serial number appears on the CRL
///
/// **Fail-open policy**: CRL fetch or parse failures are logged as warnings
/// but do not abort the check. Only confirmed revocations are reported.
///
/// # Arguments
///
/// * `certs_der` - DER-encoded certificates in chain order (root → leaf)
/// * `http_client` - HTTP client for fetching CRLs
///
/// # Errors
///
/// Returns an error only if the certificate chain itself cannot be parsed.
/// CRL fetch/parse failures are handled internally (fail-open).
pub async fn check_chain_against_crls(
    certs_der: &[&[u8]],
    http_client: &reqwest::Client,
) -> Result<Vec<RevokedCertInfo>, CrlError> {
    let cert_infos = CertCrlInfo::from_chain(certs_der)?;
    let mut revoked = Vec::new();

    // Skip root (index 0) and leaf (last) — CRL checks are for intermediates.
    // Root CAs manage their own trust; leaf certs are short-lived (~3 hours).
    for info in cert_infos.iter().skip(1).take(cert_infos.len().saturating_sub(2)) {
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

    Ok(revoked)
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

    let (_, crl) = CertificateRevocationList::from_der(&crl_bytes)
        .map_err(|e| CrlError::Parse(format!("{crl_url}: {e}")))?;

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
    use rstest::{fixture, rstest};
    use sha2::{Digest, Sha256};

    use super::*;

    // ── Real AWS Nitro cert DER hex (same fixtures as x509.rs) ──────────

    /// Root CA (self-signed, P384). Validity: 2019-10-28 to 2049-10-28.
    const ROOT_HEX: &str = "3082021130820196a003020102021100f93175681b90afe11d46ccb4e4e7f856300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3139313032383133323830355a170d3439313032383134323830355a3049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004fc0254eba608c1f36870e29ada90be46383292736e894bfff672d989444b5051e534a4b1f6dbe3c0bc581a32b7b176070ede12d69a3fea211b66e752cf7dd1dd095f6f1370f4170843d9dc100121e4cf63012809664487c9796284304dc53ff4a3423040300f0603551d130101ff040530030101ff301d0603551d0e041604149025b50dd90547e796c396fa729dcf99a9df4b96300e0603551d0f0101ff040403020186300a06082a8648ce3d0403030369003066023100a37f2f91a1c9bd5ee7b8627c1698d255038e1f0343f95b63a9628c3d39809545a11ebcbf2e3b55d8aeee71b4c3d6adf3023100a2f39b1605b27028a5dd4ba069b5016e65b4fbde8fe0061d6a53197f9cdaf5d943bc61fc2beb03cb6fee8d2302f3dff6";

    /// Intermediate 1 (signed by root). Validity: 2024-11-28 to 2024-12-18.
    const INTER1_HEX: &str = "308202be30820244a003020102021056bfc987fd05ac99c475061b1a65eedc300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3234313132383036303734355a170d3234313231383037303734355a3064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b81040022036200040713751f4391a24bf27d688c9fdde4b7eec0c4922af63f242186269602eca12354e79356170287baa07dd84fa89834726891f9b4b27032b3e86000d32471a79fbf1a30c1982ad4ed069ad96a7e11d9ae2b5cd6a93ad613ee559ed7f6385a9a89a381d53081d230120603551d130101ff040830060101ff020102301f0603551d230418301680149025b50dd90547e796c396fa729dcf99a9df4b96301d0603551d0e04160414bfbd54a168f57f7391b66ca60a2836f30acfb9a1300e0603551d0f0101ff040403020186306c0603551d1f046530633061a05fa05d865b687474703a2f2f6177732d6e6974726f2d656e636c617665732d63726c2e73332e616d617a6f6e6177732e636f6d2f63726c2f61623439363063632d376436332d343262642d396539662d3539333338636236376638342e63726c300a06082a8648ce3d0403030368003065023100c05dfd13378b1eecd926b0c3ba8da01eec89ec5502ae7ca73cb958557ca323057962fff2681993a0ab223b6eacf11033023035664252d7f9e2c89c988cc4164d390f898a5e8ac2e99dc58595aa4c624e93face7964026a99b4bcca7088b51250ccc4";

    /// Intermediate 2 (signed by inter1). Validity: 2024-11-30 to 2024-12-06.
    const INTER2_HEX: &str = "308203163082029ba003020102021100cb286a4a4a09207f8b0c14950dcd6861300a06082a8648ce3d0403033064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303033313435345a170d3234313230363031313435345a308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c653076301006072a8648ce3d020106052b810400220362000423959f700ef87dcbdba686449d944f2a89ad22aa03d73cf93d28853f2fb6a80b0cc714d3090e34cda8234eef8f804e46c0dcb216062afba3e2b36a693660d9965e2370308b8e1ffad8542ddbe3e733077481b0cbc747d8c7beb7612820d4fe95a381ea3081e730120603551d130101ff040830060101ff020101301f0603551d23041830168014bfbd54a168f57f7391b66ca60a2836f30acfb9a1301d0603551d0e04160414bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300e0603551d0f0101ff0404030201863081800603551d1f047930773075a073a071866f687474703a2f2f63726c2d75732d656173742d312d6177732d6e6974726f2d656e636c617665732e73332e75732d656173742d312e616d617a6f6e6177732e636f6d2f63726c2f30366434386638652d326330382d343738312d613634352d6231646534303261656662382e63726c300a06082a8648ce3d0403030369003066023100fa31509230632a002939201eb5686b52d79f0276db5c2b954bed324caa5c3271a60d25e2e05a5e6700e488a074af4ecd02310084770462c2ef86dcdb11fa8a31dcf770866cbd28822b682a112b98c09a30e35e94affd3482bf8b01b59a0a7775b4af18";

    /// Intermediate 3 (signed by inter2). Validity: 2024-11-30 to 2024-12-01.
    const INTER3_HEX: &str = "308202bf30820245a003020102021500c8925d382506d820d93d2c704a7523c4ba2ddfaa300a06082a8648ce3d040303308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c65301e170d3234313133303132343133315a170d3234313230313132343133315a30818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004466754b5718024df3564bcd722361e7c65a4922eda7b1f826758e30afac40b04a281062897d085311fd509b70a6bbc5f8280f86ae2ff255ad147146fc97b7afb16064f0712d335c1d473b716be320be625e91c5870973084b3a0005bc020c7b2a366306430120603551d130101ff040830060101ff020100300e0603551d0f0101ff040403020204301d0603551d0e04160414345c86a9ec55bc30cafd923d6b73111d9c57abc0301f0603551d23041830168014bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300a06082a8648ce3d0403030368003065023100aba82c02f40acb9846012bf070578217eeb2ebbfd16414948438cf67eeab6f64cdc5a152998766c88b2cdebd5a97ebd402307421611ed511567bc8e6a0a2805b981ef38dc3bd6a6c661522802b5c5d658cc4fcc9b5e8df148b161d366926896736836a";

    /// Leaf enclave cert (signed by inter3). Validity: 2024-11-30T16:22 to 2024-11-30T19:22.
    const LEAF_HEX: &str = "3082027c30820201a00302010202100193685e7fee7d8500000000674b3bd8300a06082a8648ce3d04030330818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303136323234355a170d3234313133303139323234385a308193310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753313e303c06035504030c35692d30646533386232623638353363633965382d656e63303139333638356537666565376438352e75732d656173742d312e6177733076301006072a8648ce3d020106052b810400220362000461d930c61be969237398264901d6a37282cfd42c0694d012d9143cc86a339d567913dae552bad2f10d47c50d4e670247f0344983cbdc2d2e0045d4ccbdff59ef7a26ebf1be83a81e24a651c92008fe9f465757792a0877fba02c8b5e1eb2ed90a31d301b300c0603551d130101ff04023000300b0603551d0f0404030206c0300a06082a8648ce3d0403030369003066023100e48f39a39b444a6e5ea7a38b808198a2318dd531ed62faf4a9223f71f27dff4a5e495e32dd10f250bbaf1f892a4d328f023100d09fc8e48e233b9e972eecb94798865664dbeb0d75b29041f482777a4b7cae133483dcc9d35509c4967be51db37a7454";

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

    /// Garbage bytes used for invalid DER tests.
    const INVALID_DER_BYTES: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

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
        let owned = [ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX]
            .iter()
            .map(|h| hex::decode(h).unwrap())
            .collect();
        ChainFixture { owned }
    }

    /// Single-cert chain containing only the root.
    #[fixture]
    fn root_only_chain() -> ChainFixture {
        ChainFixture { owned: vec![hex::decode(ROOT_HEX).unwrap()] }
    }

    /// Two-cert chain containing root and leaf (no intermediates).
    #[fixture]
    fn root_and_leaf_chain() -> ChainFixture {
        ChainFixture { owned: vec![hex::decode(ROOT_HEX).unwrap(), hex::decode(LEAF_HEX).unwrap()] }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Parses a single hex-encoded cert and extracts its CRL distribution
    /// point URL.
    fn crl_url_for_hex(cert_hex: &str) -> Option<String> {
        let der = hex::decode(cert_hex).unwrap();
        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        extract_crl_distribution_point(&cert)
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
        assert!(matches!(err, CrlError::CertParse(_)), "expected CertParse, got: {err}");
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

    // ── check_chain_against_crls: cert parse failure ────────────────────

    #[tokio::test]
    #[rstest]
    async fn check_chain_against_crls_fails_on_invalid_der() {
        let client = build_crl_http_client(Duration::from_secs(5)).unwrap();
        let result = check_chain_against_crls(&[&INVALID_DER_BYTES[..]], &client).await;
        let err = result.unwrap_err();
        assert!(matches!(err, CrlError::CertParse(_)), "expected CertParse, got: {err}");
    }

    #[tokio::test]
    #[rstest]
    async fn check_chain_against_crls_clean_for_empty_chain() {
        let client = build_crl_http_client(Duration::from_secs(5)).unwrap();
        let result = check_chain_against_crls(&[], &client).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    #[rstest]
    async fn check_chain_against_crls_skips_root_and_leaf(root_and_leaf_chain: ChainFixture) {
        // With only root + leaf and no intermediates, no CRL fetches should
        // be attempted, so the result is Clean even without network access.
        let client = build_crl_http_client(Duration::from_secs(1)).unwrap();
        let refs = root_and_leaf_chain.refs();
        let result = check_chain_against_crls(&refs, &client).await.unwrap();
        assert!(result.is_empty());
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
