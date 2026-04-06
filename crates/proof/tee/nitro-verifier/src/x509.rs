//! X.509 certificate chain validation for AWS Nitro Enclave attestations.
//!
//! Verifies the certificate chain from root CA through intermediates to the
//! leaf enclave certificate. Only P384/ECDSA-SHA384 is supported — this is
//! the algorithm used by all Nitro Enclave certificate chains and enforced
//! by `CertManager.sol` on-chain.
//!
//! Includes M-01 audit fixes: `basicConstraints`, `keyUsage`, path length
//! constraints, subject/issuer chain consistency, and validity period checks.

use alloy_primitives::B256;
use p384::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use sha2::{Digest, Sha256};
use x509_parser::{
    certificate::X509Certificate,
    oid_registry::{OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_SIG_ECDSA_WITH_SHA384},
    prelude::FromDer,
};

use crate::{Result, VerifierError};

/// Computes accumulated path digests for a chain of DER-encoded certificates.
///
/// Returns one `B256` per certificate, where:
/// - `digests[0] = sha256(certs_der[0])`
/// - `digests[i] = sha256(digests[i-1] || sha256(certs_der[i]))` for `i > 0`
///
/// This mirrors the on-chain `NitroEnclaveVerifier` path digest accumulation
/// used for intermediate certificate caching. Both this function and the
/// Solidity implementation must produce identical digests for the same input.
///
/// Returns an empty `Vec` for an empty input.
pub fn compute_path_digests(certs_der: &[&[u8]]) -> Vec<B256> {
    let mut digests = Vec::with_capacity(certs_der.len());
    let mut path_digest = B256::ZERO;

    for (i, der) in certs_der.iter().enumerate() {
        let cert_digest = B256::from_slice(Sha256::digest(der).as_slice());
        if i == 0 {
            path_digest = cert_digest;
        } else {
            let mut hasher = Sha256::new();
            hasher.update(path_digest.as_slice());
            hasher.update(cert_digest.as_slice());
            path_digest = B256::from_slice(hasher.finalize().as_slice());
        }
        digests.push(path_digest);
    }

    digests
}

/// Parsed DER certificate with its raw bytes.
#[derive(Debug)]
struct ParsedCert<'a> {
    /// The parsed x509 certificate.
    cert: X509Certificate<'a>,
    /// The original DER bytes (for hashing).
    der: &'a [u8],
}

/// A certificate chain validator for Nitro Enclave attestations.
///
/// Parses DER-encoded certificates and verifies the chain from root to leaf,
/// including signature verification (P384/ECDSA-SHA384), x509 content
/// validation (M-01 audit fixes), and certificate digest accumulation.
#[derive(Debug)]
pub struct CertChain<'a> {
    /// Parsed certificates in chain order: root → intermediates → leaf.
    certs: Vec<ParsedCert<'a>>,
}

impl<'a> CertChain<'a> {
    /// Parses DER-encoded certificates into a certificate chain.
    ///
    /// The certificates must be in chain order: root → intermediates → leaf.
    pub fn from_der(certs: &[&'a [u8]]) -> Result<Self> {
        if certs.is_empty() {
            return Err(VerifierError::X509Parse("empty certificate chain".into()));
        }

        let parsed = certs
            .iter()
            .enumerate()
            .map(|(i, der)| {
                let (_, cert) = X509Certificate::from_der(der)
                    .map_err(|e| VerifierError::X509Parse(format!("certificate {i}: {e}")))?;
                Ok(ParsedCert { cert, der })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self { certs: parsed })
    }

    /// Verifies the certificate chain and returns accumulated cert digests
    /// paired with expiry timestamps.
    ///
    /// Skips verification of the first `trusted_prefix_len` certificates
    /// (they are already trusted on-chain). Verifies from `trusted_prefix_len`
    /// through the leaf certificate.
    ///
    /// Returns `(Vec<B256>, Vec<u64>)`:
    /// - Accumulated path digests (one per cert), matching the on-chain
    ///   `VerifierJournal.certs` field.
    /// - Certificate expiry timestamps (`notAfter` as seconds since epoch),
    ///   matching the on-chain `VerifierJournal.certExpiries` field.
    pub fn verify_chain(
        &self,
        trusted_prefix_len: usize,
        timestamp: u64,
    ) -> Result<(Vec<B256>, Vec<u64>)> {
        if trusted_prefix_len > self.certs.len() {
            return Err(VerifierError::CertificateVerification(format!(
                "trusted prefix length {trusted_prefix_len} exceeds chain length {}",
                self.certs.len()
            )));
        }

        // Compute accumulated path digests for all certs (including trusted
        // prefix). This is pure hashing and does not depend on validation.
        let der_refs: Vec<&[u8]> = self.certs.iter().map(|c| c.der).collect();
        let digests = compute_path_digests(&der_refs);

        let mut expiries = Vec::with_capacity(self.certs.len());

        for (i, parsed) in self.certs.iter().enumerate() {
            // Extract notAfter as seconds since epoch for on-chain expiry tracking.
            let not_after = parsed.cert.validity().not_after.timestamp();
            let not_after_secs = u64::try_from(not_after).map_err(|_| {
                VerifierError::CertificateVerification(format!(
                    "certificate {i}: notAfter timestamp is negative ({not_after})"
                ))
            })?;
            expiries.push(not_after_secs);

            // Skip validation for trusted prefix certs.
            if i < trusted_prefix_len {
                continue;
            }

            // A cert is a leaf only if it's the last in a multi-cert chain.
            // A single-cert chain (root only) is treated as CA, not leaf.
            let is_leaf = i == self.certs.len() - 1 && self.certs.len() > 1;

            // Validate x509 content (M-01 audit fixes).
            Self::validate_cert_content(&parsed.cert, is_leaf)?;

            // Validate validity period.
            Self::check_validity(&parsed.cert, timestamp, i)?;

            // Validate issuer/subject chain consistency and path length.
            if i > 0 {
                let parent_cert = &self.certs[i - 1].cert;

                if parsed.cert.issuer() != parent_cert.subject() {
                    return Err(VerifierError::CertificateVerification(format!(
                        "certificate {i}: issuer does not match parent subject"
                    )));
                }

                // Check path length constraint from parent's basicConstraints.
                let parent_bc = parent_cert.basic_constraints().map_err(|e| {
                    VerifierError::CertificateVerification(format!(
                        "certificate {}: parent basicConstraints parse: {e}",
                        i - 1
                    ))
                })?;
                if let Some(bc) = parent_bc {
                    let remaining = self.certs.len() - 1 - i;
                    if let Some(max) = bc.value.path_len_constraint
                        && (remaining as u32) > max
                    {
                        return Err(VerifierError::CertificateVerification(format!(
                            "certificate {i}: path length constraint violated \
                             (max={max}, remaining intermediates={remaining})"
                        )));
                    }
                }
            }

            // Verify signature (parent signs child, root is self-signed).
            let parent = if i == 0 { &parsed.cert } else { &self.certs[i - 1].cert };
            Self::verify_signature(parent, parsed, i)?;
        }

        Ok((digests, expiries))
    }

    /// Returns the public key bytes from the leaf certificate.
    pub fn leaf_public_key(&self) -> Result<&[u8]> {
        let leaf = self.certs.last().ok_or_else(|| {
            VerifierError::CertificateVerification("empty certificate chain".into())
        })?;
        Ok(leaf.cert.public_key().subject_public_key.data.as_ref())
    }

    /// Validates x509 certificate content (M-01 audit checks).
    ///
    /// For CA certificates (non-leaf):
    /// - `basicConstraints` must exist with `cA=true`
    /// - `keyUsage` must include `keyCertSign`
    ///
    /// For the leaf certificate:
    /// - `basicConstraints`: `cA` must be `false` (or absent)
    /// - `keyUsage`: must include `digital_signature` if present
    fn validate_cert_content(cert: &X509Certificate<'_>, is_leaf: bool) -> Result<()> {
        // Check algorithm is ECDSA-SHA384 over P384.
        let sig_oid = cert.signature_algorithm.oid();
        if *sig_oid != OID_SIG_ECDSA_WITH_SHA384 {
            return Err(VerifierError::CertificateVerification(format!(
                "unsupported signature algorithm: {sig_oid}"
            )));
        }

        let pk_algorithm = &cert.public_key().algorithm;
        if pk_algorithm.algorithm != OID_KEY_TYPE_EC_PUBLIC_KEY {
            return Err(VerifierError::CertificateVerification(format!(
                "unsupported public key algorithm: {}",
                pk_algorithm.algorithm
            )));
        }

        // Verify the EC curve is P-384 (secp384r1). The algorithm parameters
        // OID must be 1.3.132.0.34. Without this check, a P-256 key would pass
        // the above EC key type check but use a weaker curve.
        let curve_oid =
            pk_algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok()).ok_or_else(|| {
                VerifierError::CertificateVerification(
                    "EC public key missing curve parameters OID".into(),
                )
            })?;
        if curve_oid != OID_NIST_EC_P384 {
            return Err(VerifierError::CertificateVerification(format!(
                "unsupported EC curve: {curve_oid} (expected P-384)"
            )));
        }

        // Parse extensions.
        let basic_constraints = cert.basic_constraints().map_err(|e| {
            VerifierError::CertificateVerification(format!("basicConstraints parse: {e}"))
        })?;

        let key_usage = cert
            .key_usage()
            .map_err(|e| VerifierError::CertificateVerification(format!("keyUsage parse: {e}")))?;

        if is_leaf {
            // Leaf: cA must NOT be true.
            if let Some(bc) = basic_constraints
                && bc.value.ca
            {
                return Err(VerifierError::CertificateVerification(
                    "leaf certificate has cA=true".into(),
                ));
            }
            // Leaf: keyUsage should include digitalSignature if present.
            if let Some(ku) = key_usage
                && !ku.value.digital_signature()
            {
                return Err(VerifierError::CertificateVerification(
                    "leaf certificate keyUsage missing digitalSignature".into(),
                ));
            }
        } else {
            // CA: basicConstraints must exist with cA=true.
            let bc = basic_constraints.ok_or_else(|| {
                VerifierError::CertificateVerification(
                    "CA certificate missing basicConstraints".into(),
                )
            })?;
            if !bc.value.ca {
                return Err(VerifierError::CertificateVerification(
                    "CA certificate has cA=false".into(),
                ));
            }
            // CA: keyUsage must exist and include keyCertSign.
            let ku = key_usage.ok_or_else(|| {
                VerifierError::CertificateVerification("CA certificate missing keyUsage".into())
            })?;
            if !ku.value.key_cert_sign() {
                return Err(VerifierError::CertificateVerification(
                    "CA certificate keyUsage missing keyCertSign".into(),
                ));
            }
        }

        Ok(())
    }

    /// Checks certificate validity period against a Unix timestamp (milliseconds).
    fn check_validity(
        cert: &X509Certificate<'_>,
        timestamp_ms: u64,
        cert_index: usize,
    ) -> Result<()> {
        let timestamp_secs = (timestamp_ms / 1000) as i64;

        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();

        if timestamp_secs < not_before {
            return Err(VerifierError::CertificateVerification(format!(
                "certificate {cert_index}: not yet valid (notBefore={not_before}, \
                 timestamp={timestamp_secs})"
            )));
        }

        if timestamp_secs > not_after {
            return Err(VerifierError::CertificateVerification(format!(
                "certificate {cert_index}: expired (notAfter={not_after}, \
                 timestamp={timestamp_secs})"
            )));
        }

        Ok(())
    }

    /// Verifies that `parent` signed the `child` certificate using P384/ECDSA-SHA384.
    fn verify_signature(
        parent: &X509Certificate<'_>,
        child: &ParsedCert<'_>,
        child_index: usize,
    ) -> Result<()> {
        // Extract the parent's public key (uncompressed P384 point).
        let pk_bytes = parent.public_key().subject_public_key.data.as_ref();
        let verifying_key = VerifyingKey::from_sec1_bytes(pk_bytes).map_err(|e| {
            VerifierError::SignatureVerification(format!(
                "certificate {child_index}: invalid parent public key: {e}"
            ))
        })?;

        // The signature on the child's TBS (to-be-signed) certificate.
        let sig_bytes = child.cert.signature_value.data.as_ref();
        let signature = Signature::from_der(sig_bytes).map_err(|e| {
            VerifierError::SignatureVerification(format!(
                "certificate {child_index}: invalid DER signature: {e}"
            ))
        })?;

        // The TBS data to verify.
        let tbs = child.cert.tbs_certificate.as_ref();

        verifying_key.verify(tbs, &signature).map_err(|e| {
            VerifierError::SignatureVerification(format!(
                "certificate {child_index}: signature verification failed: {e}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::{fixture, rstest};

    use super::*;

    // ── Real AWS Nitro cert DER hex (from NitroValidator.t.sol) ──────────

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

    /// Attestation timestamp (within all cert validity windows).
    /// 0x000001937de1c543 = 1732931765571 ms = 2024-11-30T16:22:45Z.
    const VALID_TIMESTAMP_MS: u64 = 0x000001937de1c543;

    /// Expected notAfter timestamps (seconds since epoch) for each cert in the
    /// real Nitro chain, extracted from the X.509 validity fields above.
    /// Order: root, intermediate1, intermediate2, intermediate3, leaf.
    const EXPECTED_EXPIRIES: [u64; 5] = [
        2519044085, // Root CA: 2049-10-28T14:28:05Z
        1734505665, // Intermediate 1: 2024-12-18T07:07:45Z
        1733447694, // Intermediate 2: 2024-12-06T01:14:54Z
        1733056891, // Intermediate 3: 2024-12-01T12:41:31Z
        1732994568, // Leaf: 2024-11-30T19:22:48Z
    ];

    // ── Fixtures ────────────────────────────────────────────────────────

    /// Full 5-cert chain (root → 3 intermediates → leaf) from real Nitro attestation.
    #[fixture]
    fn full_chain_der() -> Vec<Vec<u8>> {
        [ROOT_HEX, INTER1_HEX, INTER2_HEX, INTER3_HEX, LEAF_HEX]
            .iter()
            .map(|h| hex::decode(h).unwrap())
            .collect()
    }

    /// Root-only DER bytes.
    #[fixture]
    fn root_der() -> Vec<u8> {
        hex::decode(ROOT_HEX).unwrap()
    }

    // ── Happy-path tests ────────────────────────────────────────────────

    #[rstest]
    fn root_ca_self_signed_verifies(root_der: Vec<u8>) {
        let certs = vec![root_der.as_slice()];
        let chain = CertChain::from_der(&certs).unwrap();
        // Root validity: 2019-10-28 to 2049-10-28.
        let (digests, expiries) = chain.verify_chain(0, 1_700_000_000_000).unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(expiries.len(), 1);
        let expected = B256::from_slice(Sha256::digest(&root_der).as_slice());
        assert_eq!(digests[0], expected);
        assert_eq!(expiries[0], EXPECTED_EXPIRIES[0]);
    }

    #[rstest]
    fn full_chain_verifies_all_untrusted(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        let (digests, expiries) = chain.verify_chain(0, VALID_TIMESTAMP_MS).unwrap();
        assert_eq!(digests.len(), 5);
        assert_eq!(expiries.len(), 5);
    }

    #[rstest]
    #[case::root_trusted(1)]
    #[case::root_and_inter1_trusted(2)]
    #[case::root_inter1_inter2_trusted(3)]
    #[case::all_but_leaf_trusted(4)]
    #[case::all_trusted(5)]
    fn full_chain_with_trusted_prefix(full_chain_der: Vec<Vec<u8>>, #[case] prefix_len: usize) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        let (digests, expiries) = chain.verify_chain(prefix_len, VALID_TIMESTAMP_MS).unwrap();
        assert_eq!(digests.len(), 5);
        assert_eq!(expiries.len(), 5);
        // Structural invariant: each child cert expires at or before its parent
        // (AWS issues progressively shorter-lived certs down the chain).
        for i in 1..expiries.len() {
            assert!(
                expiries[i] <= expiries[i - 1],
                "cert {i} expires after its parent: {} > {}",
                expiries[i],
                expiries[i - 1]
            );
        }
    }

    #[rstest]
    fn digests_are_accumulated_path_hashes(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        let (digests, _expiries) = chain.verify_chain(5, 0).unwrap();

        // First digest = sha256(root_der).
        let root_hash = B256::from_slice(Sha256::digest(&full_chain_der[0]).as_slice());
        assert_eq!(digests[0], root_hash);

        // Second digest = sha256(root_hash || sha256(inter1_der)).
        let inter1_hash = B256::from_slice(Sha256::digest(&full_chain_der[1]).as_slice());
        let mut hasher = Sha256::new();
        hasher.update(root_hash.as_slice());
        hasher.update(inter1_hash.as_slice());
        let expected = B256::from_slice(hasher.finalize().as_slice());
        assert_eq!(digests[1], expected);
    }

    #[rstest]
    fn leaf_public_key_is_extractable(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        let pk = chain.leaf_public_key().unwrap();
        // P384 uncompressed point = 0x04 || 48-byte x || 48-byte y = 97 bytes.
        assert_eq!(pk.len(), 97);
        assert_eq!(pk[0], 0x04);
    }

    // ── Parsing error tests ─────────────────────────────────────────────

    #[rstest]
    fn empty_chain_rejected() {
        assert!(CertChain::from_der(&[]).is_err());
    }

    #[rstest]
    fn invalid_der_rejected() {
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let result = CertChain::from_der(&[&garbage]);
        assert!(result.is_err());
    }

    // ── Trusted prefix boundary tests ───────────────────────────────────

    #[rstest]
    fn trusted_prefix_skips_validation(root_der: Vec<u8>) {
        let certs = vec![root_der.as_slice()];
        let chain = CertChain::from_der(&certs).unwrap();
        // All trusted → timestamp 0 is fine (validation skipped).
        let (digests, expiries) = chain.verify_chain(1, 0).unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(expiries.len(), 1);
    }

    #[rstest]
    fn trusted_prefix_exceeding_chain_length_fails(root_der: Vec<u8>) {
        let certs = vec![root_der.as_slice()];
        let chain = CertChain::from_der(&certs).unwrap();
        assert!(chain.verify_chain(5, 0).is_err());
    }

    // ── M-01: Validity period checks ────────────────────────────────────

    #[rstest]
    fn expired_cert_rejected(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        // Far future: all intermediate certs are expired.
        let far_future_ms = 2_000_000_000_000;
        let err = chain.verify_chain(0, far_future_ms).unwrap_err();
        assert!(err.to_string().contains("expired"), "expected expiry error, got: {err}");
    }

    #[rstest]
    fn not_yet_valid_cert_rejected(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        // Far past: intermediate certs are not yet valid (root is valid from 2019).
        // Start from index 1 to skip root (root valid since 2019).
        let far_past_ms = 1_000_000_000_000; // 2001
        let err = chain.verify_chain(1, far_past_ms).unwrap_err();
        assert!(
            err.to_string().contains("not yet valid"),
            "expected not-yet-valid error, got: {err}"
        );
    }

    // ── M-01: Chain ordering ────────────────────────────────────────────

    #[rstest]
    fn wrong_chain_order_rejected(full_chain_der: Vec<Vec<u8>>) {
        // Swap intermediate1 and intermediate2 → issuer/subject mismatch.
        let refs: Vec<&[u8]> = vec![
            &full_chain_der[0],
            &full_chain_der[2], // inter2 where inter1 should be
            &full_chain_der[1], // inter1 where inter2 should be
            &full_chain_der[3],
            &full_chain_der[4],
        ];
        let chain = CertChain::from_der(&refs).unwrap();
        let err = chain.verify_chain(0, VALID_TIMESTAMP_MS).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("issuer") || msg.contains("signature"),
            "expected issuer or signature error, got: {msg}"
        );
    }

    // ── M-01: Tampered cert rejected ────────────────────────────────────

    #[rstest]
    fn tampered_cert_signature_fails(full_chain_der: Vec<Vec<u8>>) {
        let mut tampered = full_chain_der;
        // Flip a byte in the middle of inter1 to corrupt it.
        let mid = tampered[1].len() / 2;
        tampered[1][mid] ^= 0xFF;
        let refs: Vec<&[u8]> = tampered.iter().map(|c| c.as_slice()).collect();
        // Parsing may succeed (DER is tolerant) but signature verification must fail.
        if let Ok(chain) = CertChain::from_der(&refs) {
            assert!(chain.verify_chain(0, VALID_TIMESTAMP_MS).is_err());
        }
    }

    // ── Certificate expiry extraction ───────────────────────────────────

    #[rstest]
    fn expiries_match_known_cert_validity(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        // All trusted — skip validation so we can inspect expiries regardless of timestamp.
        let (_digests, expiries) = chain.verify_chain(5, 0).unwrap();
        assert_eq!(expiries.len(), EXPECTED_EXPIRIES.len());

        // Verify each cert's notAfter against the known X.509 validity periods.
        for (i, (&actual, &expected)) in expiries.iter().zip(EXPECTED_EXPIRIES.iter()).enumerate() {
            assert_eq!(actual, expected, "expiry mismatch at cert index {i}");
        }
    }

    #[rstest]
    fn expiries_returned_even_for_trusted_prefix(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();

        // Trusted prefix = 4: root + 3 intermediates cached, only leaf validated.
        let (_digests, expiries_prefix4) = chain.verify_chain(4, VALID_TIMESTAMP_MS).unwrap();
        // All trusted: skip all validation.
        let (_digests, expiries_all) = chain.verify_chain(5, 0).unwrap();

        // Expiries must be identical regardless of trusted prefix length,
        // since notAfter is extracted unconditionally for all certs.
        assert_eq!(expiries_prefix4, expiries_all);
    }

    #[rstest]
    fn expiries_length_matches_digests_length(full_chain_der: Vec<Vec<u8>>) {
        let refs: Vec<&[u8]> = full_chain_der.iter().map(|c| c.as_slice()).collect();
        let chain = CertChain::from_der(&refs).unwrap();
        let (digests, expiries) = chain.verify_chain(0, VALID_TIMESTAMP_MS).unwrap();
        assert_eq!(digests.len(), expiries.len());
    }
}
