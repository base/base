//! CRL (Certificate Revocation List) checking for AWS Nitro intermediate
//! certificates.
//!
//! Checks are fail-closed. A certificate whose CRL cannot be fetched and parsed
//! is reported as indeterminate rather than clean, so a partial failure in a
//! multi-certificate chain can never collapse into a clean result.

use std::{fmt, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use tracing::{debug, warn};
use x509_parser::{
    certificate::X509Certificate,
    extensions::ParsedExtension,
    prelude::{FromDer, GeneralName},
    revocation_list::CertificateRevocationList,
};

use crate::{CertKind, CertPlan};

const CRL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// Issuer/serial identity used by the hinted `CertManager`.
    pub revocation_id: B256,
}

impl CertCrlInfo {
    /// Extracts CRL information from the non-root CA steps in a hinted registration plan.
    ///
    /// # Errors
    ///
    /// Returns an error if any CA certificate cannot be parsed from DER.
    pub fn from_cert_plans(certs: &[CertPlan]) -> Result<Vec<Self>, CrlError> {
        certs
            .iter()
            .enumerate()
            .filter(|(_, cert)| cert.kind == CertKind::Ca)
            .map(|(index, cert_plan)| {
                let (remaining, cert) =
                    X509Certificate::from_der(&cert_plan.cert).map_err(|e| {
                        CrlError(format!("certificate parse error: certificate {}: {e}", index + 1))
                    })?;
                if !remaining.is_empty() {
                    return Err(CrlError(format!(
                        "certificate parse error: certificate {}: trailing DER data ({} bytes)",
                        index + 1,
                        remaining.len()
                    )));
                }
                Ok(Self {
                    index: index + 1,
                    serial_number: cert.tbs_certificate.serial.to_bytes_be(),
                    crl_url: Self::extract_crl_distribution_point(&cert),
                    revocation_id: cert_plan.revocation_id,
                })
            })
            .collect()
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
}

/// A certificate confirmed present on its CRL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokedCert {
    /// Position of the certificate in the chain.
    pub index: usize,
    /// Issuer/serial identity used by the hinted `CertManager`.
    pub revocation_id: B256,
}

/// A certificate whose CRL could not be fetched and parsed, leaving its
/// revocation status unknown.
#[derive(Debug)]
pub struct IndeterminateCert {
    /// Position of the certificate in the chain.
    pub index: usize,
    /// Issuer/serial identity used by the hinted `CertManager`.
    pub revocation_id: B256,
    /// Why the check did not complete.
    pub error: CrlError,
}

/// CRL classification of one certificate chain.
///
/// Confirmed revocations and indeterminate checks are reported separately so
/// that callers can fail closed on either without conflating them.
#[derive(Debug, Default)]
pub struct CrlChainStatus {
    /// Certificates listed on their CRL, in chain order.
    pub revoked: Vec<RevokedCert>,
    /// Certificates whose CRL check did not complete, in chain order.
    pub indeterminate: Vec<IndeterminateCert>,
}

/// Source of AWS Nitro certificate revocation status.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CrlSource: fmt::Debug + Send + Sync {
    /// Classifies every CA certificate in `certs` against its CRL distribution point.
    ///
    /// A certificate with no CRL distribution point has no applicable CRL and is
    /// treated as clean.
    ///
    /// # Errors
    ///
    /// Returns an error if the certificates cannot be parsed, which leaves the
    /// revocation status of the whole chain unknown.
    async fn check_chain(&self, certs: &[CertPlan]) -> Result<CrlChainStatus, CrlError>;
}

/// [`CrlSource`] that fetches CRLs over HTTP from AWS Nitro distribution points.
#[derive(Debug)]
pub struct CrlChecker {
    http_client: reqwest::Client,
}

impl CrlChecker {
    /// Builds a CRL checker with the default fetch timeout and redirects disabled.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new() -> Result<Self, CrlError> {
        reqwest::Client::builder()
            .timeout(CRL_FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(|http_client| Self { http_client })
            .map_err(|e| CrlError(format!("failed to build CRL HTTP client: {e}")))
    }

    fn is_allowed_crl_host(url: &str) -> bool {
        reqwest::Url::parse(url).is_ok_and(|u| {
            u.domain().is_some_and(|host| {
                host.ends_with(ALLOWED_CRL_HOST_SUFFIX) && host.contains(ALLOWED_CRL_HOST_KEYWORD)
            })
        })
    }

    async fn fetch_and_check_crl(
        &self,
        crl_url: &str,
        serial_number: &[u8],
    ) -> Result<bool, CrlError> {
        if !Self::is_allowed_crl_host(crl_url) {
            return Err(CrlError(format!(
                "CRL fetch error: {crl_url}: host not in CRL allowlist (must be *{ALLOWED_CRL_HOST_SUFFIX} \
                 containing '{ALLOWED_CRL_HOST_KEYWORD}')"
            )));
        }

        let response = self
            .http_client
            .get(crl_url)
            .send()
            .await
            .map_err(|e| CrlError(format!("CRL fetch error: {crl_url}: {e}")))?;

        if !response.status().is_success() {
            return Err(CrlError(format!(
                "CRL fetch error: {crl_url}: HTTP {}",
                response.status()
            )));
        }

        if let Some(content_length) = response.content_length()
            && content_length > MAX_CRL_RESPONSE_BYTES as u64
        {
            return Err(CrlError(format!(
                "CRL fetch error: {crl_url}: response too large ({content_length} bytes, max {MAX_CRL_RESPONSE_BYTES})"
            )));
        }

        let crl_bytes = response.bytes().await.map_err(|e| {
            CrlError(format!("CRL fetch error: {crl_url}: failed to read body: {e}"))
        })?;

        if crl_bytes.len() > MAX_CRL_RESPONSE_BYTES {
            return Err(CrlError(format!(
                "CRL fetch error: {crl_url}: response too large ({} bytes, max {MAX_CRL_RESPONSE_BYTES})",
                crl_bytes.len()
            )));
        }

        Self::crl_contains_serial(crl_url, &crl_bytes, serial_number)
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
}

#[async_trait]
impl CrlSource for CrlChecker {
    async fn check_chain(&self, certs: &[CertPlan]) -> Result<CrlChainStatus, CrlError> {
        let cert_infos = CertCrlInfo::from_cert_plans(certs)?;
        let mut status = CrlChainStatus::default();

        for info in &cert_infos {
            let Some(ref crl_url) = info.crl_url else {
                debug!(cert_index = info.index, "no CRL distribution point, skipping");
                continue;
            };

            debug!(cert_index = info.index, url = %crl_url, "fetching CRL");

            match self.fetch_and_check_crl(crl_url, &info.serial_number).await {
                Ok(true) => {
                    warn!(
                        cert_index = info.index,
                        url = %crl_url,
                        serial = %hex::encode(&info.serial_number),
                        revocation_id = %info.revocation_id,
                        "certificate found on CRL — REVOKED"
                    );
                    status
                        .revoked
                        .push(RevokedCert { index: info.index, revocation_id: info.revocation_id });
                }
                Ok(false) => {
                    debug!(cert_index = info.index, "certificate not on CRL");
                }
                Err(error) => {
                    warn!(
                        cert_index = info.index,
                        url = %crl_url,
                        error = %error,
                        "CRL check did not complete, revocation status is indeterminate"
                    );
                    status.indeterminate.push(IndeterminateCert {
                        index: info.index,
                        revocation_id: info.revocation_id,
                        error,
                    });
                }
            }
        }

        Ok(status)
    }
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

    /// Same shape as [`EMPTY_CRL_DER`] with one `revokedCertificates` entry for serial `0x2a`.
    const REVOKED_CRL_DER: [u8; 71] = hex!(
        "30453033300a06082a8648ce3d0403033000170d3234303130313030303030305a30143012"
        "02012a170d3234303130323030303030305a300a06082a8648ce3d04030303020000"
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

    /// Self-signed CA (serial `0x2a`) whose CRL distribution point is
    /// `http://crl.chain-5253.example.com/test.crl`. The host allowlist rejects it before any
    /// network I/O, which makes it a deterministic offline stand-in for an unfetchable CRL.
    const DISALLOWED_CDP_CA_HEX: &str = "308201eb30820170a00302010202012a300a06082a8648ce3d040303301d311b301906035504030c12636861696e2d353235332d746573742d6361301e170d3236303931303230353932305a170d3336303930373230353932305a301d311b301906035504030c12636861696e2d353235332d746573742d63613076301006072a8648ce3d020106052b8104002203620004123d3f319437443fac2c6d71ad156b47a94ecae0fc408256a977fa768ba24462518483187898c928a436eafc9e78f0caf946c00d9e923bc2f6d2ef1ce7143716b671ea017865512598969d64870f5c2eeaef74dd3ecea709e1fede82a2aac1b3a3818330818030120603551d130101ff040830060101ff020101300e0603551d0f0101ff040403020106303b0603551d1f043430323030a02ea02c862a687474703a2f2f63726c2e636861696e2d353235332e6578616d706c652e636f6d2f746573742e63726c301d0603551d0e041604144ac5443a5aae3c5491446c74699dbdf612790ffc300a06082a8648ce3d0403030369003066023100bfbfdda1c29627aebf0a6bce96b1491459ba4d2cbb4c053a681a5f75457f49c3aaa718cdc725162a39341e21c5f4b5c6023100a3107edf79c83883ecfc8462c3bd3f989a10c55225e8e97819f45b9e26cf9114ab02f8a41eaf188e469e0028460ce603";

    /// Leaf enclave cert. Validity: 2024-11-30T16:22 to 2024-11-30T19:22.
    const LEAF_HEX: &str = "3082027c30820201a00302010202100193685e7fee7d8500000000674b3bd8300a06082a8648ce3d04030330818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303136323234355a170d3234313133303139323234385a308193310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753313e303c06035504030c35692d30646533386232623638353363633965382d656e63303139333638356537666565376438352e75732d656173742d312e6177733076301006072a8648ce3d020106052b810400220362000461d930c61be969237398264901d6a37282cfd42c0694d012d9143cc86a339d567913dae552bad2f10d47c50d4e670247f0344983cbdc2d2e0045d4ccbdff59ef7a26ebf1be83a81e24a651c92008fe9f465757792a0877fba02c8b5e1eb2ed90a31d301b300c0603551d130101ff04023000300b0603551d0f0404030206c0300a06082a8648ce3d0403030369003066023100e48f39a39b444a6e5ea7a38b808198a2318dd531ed62faf4a9223f71f27dff4a5e495e32dd10f250bbaf1f892a4d328f023100d09fc8e48e233b9e972eecb94798865664dbeb0d75b29041f482777a4b7cae133483dcc9d35509c4967be51db37a7454";

    fn ca_plan(index: usize, cert_hex: &str) -> CertPlan {
        CertPlan {
            kind: CertKind::Ca,
            label: format!("certificate {index}"),
            cert: hex::decode(cert_hex).expect("static hex fixture decodes"),
            cert_hash: B256::repeat_byte(index as u8),
            parent_cert_hash: B256::repeat_byte(index as u8 - 1),
            revocation_id: B256::repeat_byte(index as u8 + 0x10),
        }
    }

    /// The CA steps of a hinted plan: the pinned root is not a plan step, so the
    /// chain starts at intermediate 1 and ends with the leaf.
    fn intermediate_plans() -> Vec<CertPlan> {
        let mut certs: Vec<CertPlan> = [INTER1_HEX, INTER2_HEX, INTER3_HEX]
            .into_iter()
            .enumerate()
            .map(|(offset, cert_hex)| ca_plan(offset + 1, cert_hex))
            .collect();
        certs.push(CertPlan { kind: CertKind::Leaf, ..ca_plan(4, LEAF_HEX) });
        certs
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
            let url = CertCrlInfo::extract_crl_distribution_point(&cert);
            assert_eq!(url.as_deref(), expected);
        }
    }

    #[test]
    fn extracts_cert_info_for_ca_steps_only() {
        let infos = CertCrlInfo::from_cert_plans(&intermediate_plans()).unwrap();

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
    fn invalid_ca_der_returns_cert_parse_error() {
        let mut certs = intermediate_plans();
        certs[1].cert = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let err = CertCrlInfo::from_cert_plans(&certs).unwrap_err();

        assert!(
            err.to_string().contains("certificate parse error"),
            "expected certificate parse error, got: {err}"
        );
    }

    #[test]
    fn trailing_der_certificate_alias_returns_cert_parse_error() {
        let mut certs = intermediate_plans();
        certs[1].cert.extend_from_slice(b"chain-4256-trailing-der");

        let err = CertCrlInfo::from_cert_plans(&certs).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("trailing DER data"), "expected trailing DER error, got: {msg}");
    }

    /// A chain the checker cannot parse leaves every certificate's status unknown, so
    /// `check_chain` must surface an error rather than an empty (clean) status.
    #[tokio::test]
    async fn unparseable_chain_is_an_error_not_a_clean_status() {
        let mut certs = intermediate_plans();
        certs[0].cert = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let err = CrlChecker::new().unwrap().check_chain(&certs).await.unwrap_err();

        assert!(
            err.to_string().contains("certificate parse error"),
            "expected certificate parse error, got: {err}"
        );
    }

    /// A CA whose CRL cannot be fetched must land in `indeterminate` on its own, without
    /// dragging in the CA that has no distribution point and without reporting the chain clean.
    #[tokio::test]
    async fn unfetchable_distribution_point_is_indeterminate_not_clean() {
        let certs = vec![ca_plan(1, INTER3_HEX), ca_plan(2, DISALLOWED_CDP_CA_HEX)];

        let status = CrlChecker::new().unwrap().check_chain(&certs).await.unwrap();

        assert!(status.revoked.is_empty());
        assert_eq!(
            status.indeterminate.iter().map(|cert| cert.index).collect::<Vec<_>>(),
            vec![2],
            "only the CA with an unfetchable distribution point should be indeterminate"
        );
        assert_eq!(status.indeterminate[0].revocation_id, certs[1].revocation_id);
    }

    #[tokio::test]
    async fn chain_without_distribution_points_is_clean() {
        let certs = vec![ca_plan(1, INTER3_HEX)];

        let status = CrlChecker::new().unwrap().check_chain(&certs).await.unwrap();

        assert!(status.revoked.is_empty());
        assert!(status.indeterminate.is_empty());
    }

    #[test]
    fn crl_membership_matches_only_the_listed_serial() {
        for (serial, expected) in [(&[0x2a][..], true), (&[0x2b][..], false)] {
            assert_eq!(
                CrlChecker::crl_contains_serial("test.crl", &REVOKED_CRL_DER, serial).unwrap(),
                expected,
                "serial {serial:?}"
            );
        }
        assert!(!CrlChecker::crl_contains_serial("test.crl", &EMPTY_CRL_DER, &[0x2a]).unwrap());
    }

    #[test]
    fn crl_parse_rejects_trailing_der() {
        let mut aliased = EMPTY_CRL_DER.to_vec();
        aliased.extend_from_slice(b"chain-4256-trailing-crl");
        let err = CrlChecker::crl_contains_serial("test.crl", &aliased, &[]).unwrap_err();
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
                CrlChecker::is_allowed_crl_host(url),
                expected,
                "is_allowed_crl_host({url}) should be {expected}"
            );
        }
    }
}
