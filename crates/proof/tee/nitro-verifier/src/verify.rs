//! Top-level attestation verification orchestrating COSE parsing, certificate
//! chain validation, signature verification, and content validation.
//!
//! [`AttestationVerifier::verify`] is the ZK guest entry point — called from the
//! RISC Zero guest program in CHAIN-3560. It must be deterministic with no
//! side effects.

use alloy_primitives::Bytes;
use p384::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde_bytes::ByteBuf;

use crate::{
    Result, VerifierError, VerifierInput,
    attestation::{AttestationDocument, AttestationReport},
    types::{Bytes48, Pcr, VerificationResult, VerifierJournal},
    x509::CertChain,
};

/// Attestation report verifier.
///
/// Entry point for end-to-end Nitro attestation verification including
/// COSE parsing, M-02 content validation, x509 chain validation (M-01),
/// COSE signature verification, and journal construction.
#[derive(Debug)]
pub struct AttestationVerifier;

impl AttestationVerifier {
    /// Verifies a Nitro attestation report end-to-end.
    ///
    /// 1. Parses the `COSE_Sign1` envelope and attestation document
    /// 2. Validates attestation content (M-02 checks)
    /// 3. Verifies the x509 certificate chain (M-01 checks)
    /// 4. Verifies the COSE signature against the leaf certificate's public key
    /// 5. Extracts verified data into a `VerifierJournal`
    pub fn verify(input: &VerifierInput) -> Result<VerifierJournal> {
        // 1. Parse attestation report.
        let report = AttestationReport::parse(&input.attestationReport)?;

        // 2. Validate attestation content (M-02).
        Self::validate_attestation_content(&report.doc)?;

        // 3. Extract and verify certificate chain.
        let cert_chain_der = report.cert_chain_der();
        let chain = CertChain::from_der(&cert_chain_der)?;

        let trusted_prefix_len = input.trustedCertsPrefixLen as usize;
        let certs = chain.verify_chain(trusted_prefix_len, report.doc.timestamp)?;

        // 4. Verify COSE signature against the leaf certificate's public key.
        let leaf_pk_bytes = chain.leaf_public_key()?;
        let sig_structure = report.cose.sig_structure()?;
        Self::verify_cose_signature(leaf_pk_bytes, &sig_structure, &report.cose.signature)?;

        // 5. Build the journal.
        let pcrs = report
            .doc
            .pcrs
            .iter()
            .map(|(&index, value)| Pcr { index, value: Bytes48::from(value) })
            .collect::<Vec<_>>();

        Ok(VerifierJournal {
            result: VerificationResult::Success,
            trustedCertsPrefixLen: input.trustedCertsPrefixLen,
            timestamp: report.doc.timestamp,
            certs,
            userData: Self::optional_bytes(&report.doc.user_data),
            nonce: Self::optional_bytes(&report.doc.nonce),
            publicKey: Self::optional_bytes(&report.doc.public_key),
            pcrs,
            moduleId: report.doc.module_id,
        })
    }

    /// Verifies the COSE signature over the `Sig_structure` using the leaf cert's P384 key.
    fn verify_cose_signature(
        leaf_pk_bytes: &[u8],
        sig_structure: &[u8],
        signature_bytes: &[u8],
    ) -> Result<()> {
        let verifying_key = VerifyingKey::from_sec1_bytes(leaf_pk_bytes).map_err(|e| {
            VerifierError::SignatureVerification(format!("invalid leaf public key: {e}"))
        })?;

        let signature = Signature::from_slice(signature_bytes).map_err(|e| {
            VerifierError::SignatureVerification(format!("invalid COSE signature: {e}"))
        })?;

        verifying_key.verify(sig_structure, &signature).map_err(|e| {
            VerifierError::SignatureVerification(format!("COSE signature verification failed: {e}"))
        })
    }

    /// Converts an optional `ByteBuf` to `Bytes`, defaulting to empty.
    fn optional_bytes(opt: &Option<ByteBuf>) -> Bytes {
        opt.as_ref().map_or_else(Bytes::new, |b| Bytes::copy_from_slice(b.as_ref()))
    }

    /// Validates attestation document content (M-02 audit checks).
    ///
    /// Per `NitroValidator.sol` reference:
    /// - `module_id` non-empty
    /// - `timestamp` non-zero
    /// - `digest` == `"SHA384"`
    /// - `cabundle` has >= 1 certificate
    /// - PCR count between 1 and 32, each index 0-31
    /// - `public_key`: null or 1-1024 bytes (present-but-empty is rejected, matching Solidity)
    /// - `user_data`: null or <= 512 bytes
    /// - `nonce`: null or <= 512 bytes
    fn validate_attestation_content(doc: &AttestationDocument) -> Result<()> {
        if doc.module_id.is_empty() {
            return Err(VerifierError::ContentValidation("module_id is empty".into()));
        }

        if doc.timestamp == 0 {
            return Err(VerifierError::ContentValidation("timestamp is zero".into()));
        }

        if doc.digest != "SHA384" {
            return Err(VerifierError::ContentValidation(format!(
                "unsupported digest algorithm: '{}' (expected 'SHA384')",
                doc.digest
            )));
        }

        if doc.cabundle.is_empty() {
            return Err(VerifierError::ContentValidation("cabundle is empty".into()));
        }

        // PCR count: 1-32, each index 0-31.
        // PCR sizes are statically enforced as 48 bytes by `ByteArray<48>`.
        let pcr_count = doc.pcrs.len();
        if pcr_count == 0 || pcr_count > 32 {
            return Err(VerifierError::ContentValidation(format!(
                "PCR count {pcr_count} out of range (must be 1-32)"
            )));
        }
        for &index in doc.pcrs.keys() {
            if index > 31 {
                return Err(VerifierError::ContentValidation(format!(
                    "PCR index {index} out of range (must be 0-31)"
                )));
            }
        }

        // Optional field size limits. public_key min=1 means present-but-empty is
        // rejected, matching Solidity where `pubkeyLen == 0` means absent (null).
        if let Some(pk) = &doc.public_key
            && (pk.is_empty() || pk.len() > 1024)
        {
            return Err(VerifierError::ContentValidation(format!(
                "public_key length {} out of range (1-1024)",
                pk.len()
            )));
        }
        if let Some(ud) = &doc.user_data
            && ud.len() > 512
        {
            return Err(VerifierError::ContentValidation(format!(
                "user_data length {} exceeds maximum (512)",
                ud.len()
            )));
        }
        if let Some(n) = &doc.nonce
            && n.len() > 512
        {
            return Err(VerifierError::ContentValidation(format!(
                "nonce length {} exceeds maximum (512)",
                n.len()
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_sol_types::SolValue;
    use rstest::{fixture, rstest};
    use serde_bytes::{ByteArray, ByteBuf};

    use super::*;

    /// Full attestation `COSE_Sign1` from `NitroValidator.t.sol` `test_DecodeAttestationTbs`.
    const ATTESTATION_HEX: &str = "8444a1013822a0591144a9696d6f64756c655f69647827692d30646533386232623638353363633965382d656e633031393336383565376665653764383566646967657374665348413338346974696d657374616d701b000001937de1c5436470637273b0005830ec74bfbe7f7445a6c7610e152935e028276f638042b74797b119648e13f7a3675796b721034c320f140ea001b41aeae2015830fa2593b59f3e4fc7daba5cbdddfd3449d67cd02d43bb1128885e8f38b914d081dccdb68fff6d5b7a76bcb866a18a74a302583056ba201a72e36cd051e95e5c4724c899039b711770f4d9d4fe7a1de007119a10b364badcd35e90f728a5bdc9109057230358303c9cadd84f0d027d6a5370c3de4af9179824fd6f3f02ebab723ee4439c75d8f5183e1c55f523415d44e9e6580b06655204583098bdf1bde262272618ccd73279e8ee00dd2c36974bd253de55413a25ceb2cd7221421207c2c09dde609f87481b6f6c940558300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000658300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000758300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000858300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000958300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000b58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000d58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000e58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f58300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006b63657274696669636174655902803082027c30820201a00302010202100193685e7fee7d8500000000674b3bd8300a06082a8648ce3d04030330818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303136323234355a170d3234313133303139323234385a308193310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753313e303c06035504030c35692d30646533386232623638353363633965382d656e63303139333638356537666565376438352e75732d656173742d312e6177733076301006072a8648ce3d020106052b810400220362000461d930c61be969237398264901d6a37282cfd42c0694d012d9143cc86a339d567913dae552bad2f10d47c50d4e670247f0344983cbdc2d2e0045d4ccbdff59ef7a26ebf1be83a81e24a651c92008fe9f465757792a0877fba02c8b5e1eb2ed90a31d301b300c0603551d130101ff04023000300b0603551d0f0404030206c0300a06082a8648ce3d0403030369003066023100e48f39a39b444a6e5ea7a38b808198a2318dd531ed62faf4a9223f71f27dff4a5e495e32dd10f250bbaf1f892a4d328f023100d09fc8e48e233b9e972eecb94798865664dbeb0d75b29041f482777a4b7cae133483dcc9d35509c4967be51db37a745468636162756e646c65845902153082021130820196a003020102021100f93175681b90afe11d46ccb4e4e7f856300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3139313032383133323830355a170d3439313032383134323830355a3049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004fc0254eba608c1f36870e29ada90be46383292736e894bfff672d989444b5051e534a4b1f6dbe3c0bc581a32b7b176070ede12d69a3fea211b66e752cf7dd1dd095f6f1370f4170843d9dc100121e4cf63012809664487c9796284304dc53ff4a3423040300f0603551d130101ff040530030101ff301d0603551d0e041604149025b50dd90547e796c396fa729dcf99a9df4b96300e0603551d0f0101ff040403020186300a06082a8648ce3d0403030369003066023100a37f2f91a1c9bd5ee7b8627c1698d255038e1f0343f95b63a9628c3d39809545a11ebcbf2e3b55d8aeee71b4c3d6adf3023100a2f39b1605b27028a5dd4ba069b5016e65b4fbde8fe0061d6a53197f9cdaf5d943bc61fc2beb03cb6fee8d2302f3dff65902c2308202be30820244a003020102021056bfc987fd05ac99c475061b1a65eedc300a06082a8648ce3d0403033049310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c03415753311b301906035504030c126177732e6e6974726f2d656e636c61766573301e170d3234313132383036303734355a170d3234313231383037303734355a3064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b81040022036200040713751f4391a24bf27d688c9fdde4b7eec0c4922af63f242186269602eca12354e79356170287baa07dd84fa89834726891f9b4b27032b3e86000d32471a79fbf1a30c1982ad4ed069ad96a7e11d9ae2b5cd6a93ad613ee559ed7f6385a9a89a381d53081d230120603551d130101ff040830060101ff020102301f0603551d230418301680149025b50dd90547e796c396fa729dcf99a9df4b96301d0603551d0e04160414bfbd54a168f57f7391b66ca60a2836f30acfb9a1300e0603551d0f0101ff040403020186306c0603551d1f046530633061a05fa05d865b687474703a2f2f6177732d6e6974726f2d656e636c617665732d63726c2e73332e616d617a6f6e6177732e636f6d2f63726c2f61623439363063632d376436332d343262642d396539662d3539333338636236376638342e63726c300a06082a8648ce3d0403030368003065023100c05dfd13378b1eecd926b0c3ba8da01eec89ec5502ae7ca73cb958557ca323057962fff2681993a0ab223b6eacf11033023035664252d7f9e2c89c988cc4164d390f898a5e8ac2e99dc58595aa4c624e93face7964026a99b4bcca7088b51250ccc459031a308203163082029ba003020102021100cb286a4a4a09207f8b0c14950dcd6861300a06082a8648ce3d0403033064310b3009060355040613025553310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533136303406035504030c2d636264383238303866646138623434642e75732d656173742d312e6177732e6e6974726f2d656e636c61766573301e170d3234313133303033313435345a170d3234313230363031313435345a308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c653076301006072a8648ce3d020106052b810400220362000423959f700ef87dcbdba686449d944f2a89ad22aa03d73cf93d28853f2fb6a80b0cc714d3090e34cda8234eef8f804e46c0dcb216062afba3e2b36a693660d9965e2370308b8e1ffad8542ddbe3e733077481b0cbc747d8c7beb7612820d4fe95a381ea3081e730120603551d130101ff040830060101ff020101301f0603551d23041830168014bfbd54a168f57f7391b66ca60a2836f30acfb9a1301d0603551d0e04160414bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300e0603551d0f0101ff0404030201863081800603551d1f047930773075a073a071866f687474703a2f2f63726c2d75732d656173742d312d6177732d6e6974726f2d656e636c617665732e73332e75732d656173742d312e616d617a6f6e6177732e636f6d2f63726c2f30366434386638652d326330382d343738312d613634352d6231646534303261656662382e63726c300a06082a8648ce3d0403030369003066023100fa31509230632a002939201eb5686b52d79f0276db5c2b954bed324caa5c3271a60d25e2e05a5e6700e488a074af4ecd02310084770462c2ef86dcdb11fa8a31dcf770866cbd28822b682a112b98c09a30e35e94affd3482bf8b01b59a0a7775b4af185902c3308202bf30820245a003020102021500c8925d382506d820d93d2c704a7523c4ba2ddfaa300a06082a8648ce3d040303308189313c303a06035504030c33343762313739376131663031386266302e7a6f6e616c2e75732d656173742d312e6177732e6e6974726f2d656e636c61766573310c300a060355040b0c03415753310f300d060355040a0c06416d617a6f6e310b3009060355040613025553310b300906035504080c0257413110300e06035504070c0753656174746c65301e170d3234313133303132343133315a170d3234313230313132343133315a30818e310b30090603550406130255533113301106035504080c0a57617368696e67746f6e3110300e06035504070c0753656174746c65310f300d060355040a0c06416d617a6f6e310c300a060355040b0c034157533139303706035504030c30692d30646533386232623638353363633965382e75732d656173742d312e6177732e6e6974726f2d656e636c617665733076301006072a8648ce3d020106052b8104002203620004466754b5718024df3564bcd722361e7c65a4922eda7b1f826758e30afac40b04a281062897d085311fd509b70a6bbc5f8280f86ae2ff255ad147146fc97b7afb16064f0712d335c1d473b716be320be625e91c5870973084b3a0005bc020c7b2a366306430120603551d130101ff040830060101ff020100300e0603551d0f0101ff040403020204301d0603551d0e04160414345c86a9ec55bc30cafd923d6b73111d9c57abc0301f0603551d23041830168014bbf52a3a42fdc4f301f72536b90e65aaa1b70a99300a06082a8648ce3d0403030368003065023100aba82c02f40acb9846012bf070578217eeb2ebbfd16414948438cf67eeab6f64cdc5a152998766c88b2cdebd5a97ebd402307421611ed511567bc8e6a0a2805b981ef38dc3bd6a6c661522802b5c5d658cc4fcc9b5e8df148b161d366926896736836a7075626c69635f6b657958410433a4701fa871b188983d570e2c2d8cf98fd66eb19ba8ca7617bc8e20e152a5d7f0205eae76e608ce855077e4565be69db4471ef72857253742f9602c11ff04e569757365725f64617461f6656e6f6e6365f65860874e67088943e85654beb78443c747def2c3736bf93e2b52d033b3e936a04ead91f7b5a1229a1615f237f138f64399418b8046b6e40cd93e750b58f5e1aded45ebf3f103b9ea19a9b874142b576638dad2da142254ae913664649be22e0b83f9";

    // ── Fixtures ────────────────────────────────────────────────────────

    #[fixture]
    fn attestation_input() -> VerifierInput {
        let bytes = hex::decode(ATTESTATION_HEX).unwrap();
        VerifierInput {
            trustedCertsPrefixLen: 1,
            attestationReport: Bytes::copy_from_slice(&bytes),
        }
    }

    /// A minimal valid `AttestationDocument` for M-02 unit tests.
    fn valid_doc() -> AttestationDocument {
        let mut pcrs = BTreeMap::new();
        pcrs.insert(0, ByteArray::new([0u8; 48]));
        AttestationDocument {
            module_id: "test-module".into(),
            timestamp: 1_700_000_000_000,
            digest: "SHA384".into(),
            pcrs,
            certificate: ByteBuf::from(vec![0x30]),
            cabundle: vec![ByteBuf::from(vec![0x30])],
            public_key: None,
            user_data: None,
            nonce: None,
        }
    }

    // ── End-to-end happy path ───────────────────────────────────────────

    #[rstest]
    fn verify_full_attestation_report(attestation_input: VerifierInput) {
        let journal = AttestationVerifier::verify(&attestation_input).unwrap();

        assert_eq!(journal.result, VerificationResult::Success);
        assert_eq!(journal.trustedCertsPrefixLen, 1);
        assert_eq!(journal.timestamp, 0x000001937de1c543);
        assert_eq!(journal.moduleId, "i-0de38b2b6853cc9e8-enc0193685e7fee7d85");
        assert_eq!(journal.certs.len(), 5);
        assert_eq!(journal.pcrs.len(), 16);
        assert!(journal.userData.is_empty());
        assert!(journal.nonce.is_empty());
        assert!(!journal.publicKey.is_empty());

        // ABI round-trip.
        let encoded = SolValue::abi_encode(&journal);
        let decoded = VerifierJournal::decode(&encoded).unwrap();
        assert_eq!(decoded.moduleId, journal.moduleId);
        assert_eq!(decoded.timestamp, journal.timestamp);
    }

    #[rstest]
    fn verify_with_zero_trusted_prefix() {
        let bytes = hex::decode(ATTESTATION_HEX).unwrap();
        let input = VerifierInput {
            trustedCertsPrefixLen: 0,
            attestationReport: Bytes::copy_from_slice(&bytes),
        };
        let journal = AttestationVerifier::verify(&input).unwrap();
        assert_eq!(journal.result, VerificationResult::Success);
    }

    // ── End-to-end error tests ──────────────────────────────────────────

    #[rstest]
    fn corrupted_attestation_rejected() {
        let input = VerifierInput {
            trustedCertsPrefixLen: 0,
            attestationReport: Bytes::copy_from_slice(&[0xDE, 0xAD]),
        };
        assert!(AttestationVerifier::verify(&input).is_err());
    }

    // ── M-02: validate_attestation_content unit tests ───────────────────

    #[rstest]
    fn m02_valid_doc_passes() {
        assert!(AttestationVerifier::validate_attestation_content(&valid_doc()).is_ok());
    }

    #[rstest]
    fn m02_empty_module_id_rejected() {
        let mut doc = valid_doc();
        doc.module_id = String::new();
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("module_id"));
    }

    #[rstest]
    fn m02_zero_timestamp_rejected() {
        let mut doc = valid_doc();
        doc.timestamp = 0;
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("timestamp"));
    }

    #[rstest]
    fn m02_wrong_digest_rejected() {
        let mut doc = valid_doc();
        doc.digest = "SHA256".into();
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("SHA384"));
    }

    #[rstest]
    fn m02_empty_cabundle_rejected() {
        let mut doc = valid_doc();
        doc.cabundle.clear();
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("cabundle"));
    }

    #[rstest]
    fn m02_empty_pcrs_rejected() {
        let mut doc = valid_doc();
        doc.pcrs.clear();
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("PCR count"));
    }

    #[rstest]
    fn m02_pcr_index_out_of_range_rejected() {
        let mut doc = valid_doc();
        doc.pcrs.insert(32, ByteArray::new([0u8; 48]));
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("PCR index"));
    }

    #[rstest]
    fn m02_pcr_index_31_accepted() {
        let mut doc = valid_doc();
        doc.pcrs.insert(31, ByteArray::new([0u8; 48]));
        assert!(AttestationVerifier::validate_attestation_content(&doc).is_ok());
    }

    #[rstest]
    fn m02_public_key_empty_rejected() {
        let mut doc = valid_doc();
        doc.public_key = Some(ByteBuf::from(vec![]));
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("public_key"));
    }

    #[rstest]
    fn m02_public_key_too_large_rejected() {
        let mut doc = valid_doc();
        doc.public_key = Some(ByteBuf::from(vec![0u8; 1025]));
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("public_key"));
    }

    #[rstest]
    fn m02_user_data_too_large_rejected() {
        let mut doc = valid_doc();
        doc.user_data = Some(ByteBuf::from(vec![0u8; 513]));
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("user_data"));
    }

    #[rstest]
    fn m02_nonce_too_large_rejected() {
        let mut doc = valid_doc();
        doc.nonce = Some(ByteBuf::from(vec![0u8; 513]));
        let err = AttestationVerifier::validate_attestation_content(&doc).unwrap_err();
        assert!(err.to_string().contains("nonce"));
    }

    #[rstest]
    fn m02_valid_optional_fields_accepted() {
        let mut doc = valid_doc();
        doc.public_key = Some(ByteBuf::from(vec![0x04; 65]));
        doc.user_data = Some(ByteBuf::from(vec![0u8; 512]));
        doc.nonce = Some(ByteBuf::from(vec![0u8; 256]));
        assert!(AttestationVerifier::validate_attestation_content(&doc).is_ok());
    }
}
