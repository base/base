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

    const INTER1_EXPECTED_CRL_URL: &str = "http://aws-nitro-enclaves-crl.s3.amazonaws.com/crl/ab4960cc-7d63-42bd-9e9f-59338cb67f84.crl";

    const INTER2_EXPECTED_CRL_URL: &str = "http://crl-us-east-1-aws-nitro-enclaves.s3.us-east-1.amazonaws.com/crl/06d48f8e-2c08-4781-a645-b1de402aefb8.crl";

    const EMPTY_CRL_DER: [u8; 49] = hex!(
        "302f301d300a06082a8648ce3d0403033000170d3234303130313030303030305a300a06082a8648ce3d04030303020000"
    );

    /// Real AWS Nitro root CA (self-signed, P384). Validity: 2019-10-28 to
    /// 2049-10-28.
    const ROOT_HEX: &str = "3082021130820196a003020102021100f93175681b90afe11d46ccb4e4e7f856300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3139313032383133323830355a170d3439313032383134323830355a3049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004fc0254eba608c1f36870e29ada90be46383292736e894bfff672d989444b5051e534a4b1f6dbe3c0bc581a32b7b176070ede12d69a3fea211b66e752cf7dd1dd095f6f1370f4170843d9dc100121e4cf63012809664487c9796284304dc53ff4a3423040300f0603551d130101ff040530030101ff301d0603551d0e041604149025b50dd90547e796c396fa729dcf99a9df4b96300e0603551d0f0101ff040403020186300a06082a8648ce3d0403030369003066023100a37f2f91a1c9bd5ee7b8627c1698d255038e1f0343f95b63a9628c3d39809545a11ebcbf2e3b55d8aeee71b4c3d6adf3023100a2f39b1605b27028a5dd4ba069b5016e65b4fbde8fe0061d6a53197f9cdaf5d943bc61fc2beb03cb6fee8d2302f3dff6";

    /// Intermediate 1 (signed by root). Validity: 2024-11-28 to 2024-12-18.
    const INTER1_HEX: &str = "308202be30820244a003020102021056bfc987fd05ac99c475061b1a65eedc300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3234313132383036303734355a170d3234313231383037303734355a3064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b81040022036200040713751f4391a24bf27d688c9fdde4b7eec0c4922af63f242186269602eca12354e79356170287baa07dd84fa89834726891f9b4b27032b3e86000d32471a79fbf1a30c1982ad4ed069ad96a7e11d9ae2b5cd6a93ad613ee559ed7f6385a9a89a381d53081d230120603551d130101ff040830060101ff020102301f0603551d230418301680149025b50dd90547e796c396fa729dcf99a9df4b96301d0603551d0e04160414bfbd54a168f57f7391b66ca60a2836f30acfb9a1300e0603551d0f0101ff040403020186306c0603551d1f046530633061a05fa05d865b687474703a2f2f6177732d6e6974726f2d656e636c617665732d63726c2e73332e616d617a6f6e6177732e636f6d2f63726c2f61623439363063632d376436332d343262642d396539662d3539333338636236376638342e63726c300a06082a8648ce3d0403030368003065023100c05dfd13378b1eecd926b0c3ba8da01eec89ec5502ae7ca73cb958557ca323057962fff2681993a0ab223b6eacf11033023035664252d7f9e2c89c988cc4164d390f898a5e8ac2e99dc58595aa4c624e93face7964026a99b4bcca7088b51250ccc4";

    /// Intermediate 2 (signed by inter1). Validity: 2024-11-30 to 2024-12-06.
    const INTER2_HEX: &str = "308203163082029ba003020102021100cb286a4a4a09207f8b0c14950dcd6861300a06082a8648ce3d0403033064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303033313435345a170d3234313230363031313435345a308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c653076301006072a8648ce3d020106052b810400220362000423959f700ef87dcbdba686449d944f2a89ad22aa03d73cf93d28853f2fb6a80b0cc714d3090e34cda8234eef8f804e46c0dcb216062afba3e2b36a693660d9965e2370308b8e1ffad8542ddbe3e733077481b0cbc747d8c7beb7612820d4fe95a381ea3081e730120603551d130101ff040830060101ff020101301f0603551d23041830168014bfbd54a168f57f7391b66ca60a2836f30acfb9a1301d0603551d0e04160414bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300e0603551d0f0101ff0404030201863081800603551d1f047930773075a073a071866f687474703a2f2f63726c2d75732d656173742d312d6177732d6e6974726f2d656e636c617665732e73332e75732d656173742d312e616d617a6f6e6177732e636f6d2f63726c2f30366434386638652d326330382d343738312d613634352d6231646534303261656662382e63726c300a06082a8648ce3d0403030369003066023100fa31509230632a002939201eb5686b52d79f0276db5c2b954bed324caa5c3271a60d25e2e05a5e6700e488a074af4ecd02310084770462c2ef86dcdb11fa8a31dcf770866cbd28822b682a112b98c09a30e35e94affd3482bf8b01b59a0a7775b4af18";

    /// Intermediate 3 (signed by inter2). Validity: 2024-11-30 to 2024-12-01.
    const INTER3_HEX: &str = "308202bf30820245a003020102021500c8925d382506d820d93d2c704a7523c4ba2ddfaa300a06082a8648ce3d040303308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c65301e170d3234313133303132343133315a170d3234313230313132343133315a30818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004466754b5718024df3564bcd722361e7c65a4922eda7b1f826758e30afac40b04a281062897d085311fd509b70a6bbc5f8280f86ae2ff255ad147146fc97b7afb16064f0712d335c1d473b716be320be625e91c5870973084b3a0005bc020c7b2a366306430120603551d130101ff040830060101ff020100300e0603551d0f0101ff040403020204301d0603551d0e04160414345c86a9ec55bc30cafd923d6b73111d9c57abc0301f0603551d23041830168014bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300a06082a8648ce3d0403030368003065023100aba82c02f40acb9846012bf070578217eeb2ebbfd16414948438cf67eeab6f64cdc5a152998766c88b2cdebd5a97ebd402307421611ed511567bc8e6a0a2805b981ef38dc3bd6a6c661522802b5c5d658cc4fcc9b5e8df148b161d36692689673683";

    /// Leaf enclave cert. Validity: 2024-11-30T16:22 to 2024-11-30T19:22.
    const LEAF_HEX: &str = "3082027c30820201a00302010202100193685e7fee7d8500000000674b3bd8300a06082a8648ce3d04030330818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303136323234355a170d3234313133303139323234385a308193310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753313e303c06035504030c35692d30646533386232623638353363633965382d656e63303139333638356537666565376438352e75732d656173742d312e6177733076301006072a8648ce3d020106052b810400220362000461d930c61be969237398264901d6a37282cfd42c0694d012d9143cc86a339d567913dae552bad2f10d47c50d4e670247f0344983cbdc2d2e0045d4ccbdff59ef7a26ebf1be83a81e24a651c92008fe9f465757792a0877fba02c8b5e1eb2ed90a31d301b300c0603551d130101ff04023000300b0603551d0f0404030206c0300a06082a8648ce3d0403030369003066023100e48f39a39b444a6e5ea7a38b808198a2318dd531ed62faf4a9223f71f27dff4a5e495e32dd10f250bbaf1f892a4d328f023100d09fc8e48e233b9e972eecb94798865664dbeb0d75b29041f482777a4b7cae133483dcc9d35509c4967be51db37a7454";

    fn full_chain() -> Vec<Vec<u8>> {
        [ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX]
            .into_iter()
            .map(|cert_hex| hex::decode(cert_hex).expect("static hex fixture decodes"))
            .collect()
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
            let der = hex::decode(cert_hex).expect("static hex fixture decodes");
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
        let root = hex::decode(ROOT_HEX).expect("static hex fixture decodes");
        let leaf = hex::decode(LEAF_HEX).expect("static hex fixture decodes");
        let result = CertCrlInfo::from_chain(&[&root, &[0xDE, 0xAD, 0xBE, 0xEF][..], &leaf]);
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
