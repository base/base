//! End-to-end TDX quote, collateral, policy, and journal verification.

use alloy_primitives::{Address, B256, Bytes, keccak256};
use k256::PublicKey;

use crate::{
    Result, TDXTcbStatus, TDXVerificationResult, TDXVerifierJournal, TdxCertificate, TdxCollateral,
    TdxPlatformIdentity, TdxQuote, TdxRevocationEvidence, TdxSignedCollateralBody,
    TdxVerifierError, collateral::CollateralVerifier,
};

const REPORT_DATA_CONTEXT: &[u8] = b"base-tdx-tee-prover-v1";

/// Complete explicit input to the pure TDX verifier.
#[derive(Debug)]
pub struct TdxVerifierInput {
    /// Raw Intel TDX quote bytes.
    pub quote: Bytes,
    /// Root-to-leaf PCK certificate chain for the quote attestation key.
    pub pck_certificate_chain: Vec<TdxCertificate>,
    /// TCB info collateral and QE identity collateral.
    pub collateral: TdxCollateral,
    /// CRLs or equivalent revocation evidence.
    pub revocation: TdxRevocationEvidence,
    /// Trusted Intel root CA hash expected by the onchain verifier.
    pub trusted_root_ca_hash: B256,
    /// Expected uncompressed secp256k1 signer public key: `0x04 || x || y`.
    pub expected_public_key: Bytes,
    /// Optional deterministic registrar nonce bound into the quote report data.
    pub attestation_nonce: Option<B256>,
    /// Quote collection timestamp in milliseconds since Unix epoch.
    ///
    /// This value must match the timestamp commitment in `TDREPORT.REPORTDATA`.
    pub quote_timestamp_millis: u64,
    /// Verification time in seconds since Unix epoch.
    pub verification_time: u64,
    /// Maximum accepted quote age in seconds.
    pub max_quote_age_seconds: u64,
    /// Contract TCB statuses accepted by verifier policy.
    pub allowed_tcb_statuses: Vec<TDXTcbStatus>,
}

/// Stateless TDX attestation verifier.
#[derive(Debug)]
pub struct TdxVerifier;

impl TdxVerifier {
    /// Verifies a TDX quote and collateral bundle into an onchain journal.
    pub fn verify(input: &TdxVerifierInput) -> Result<TDXVerifierJournal> {
        let quote = TdxQuote::parse(&input.quote)?;

        let (pck_leaf_key, pck_expiration) = CollateralVerifier::verify_certificate_chain(
            &input.pck_certificate_chain,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )?;
        TdxQuote::verify_qe_report(&quote, &pck_leaf_key)?;
        TdxQuote::verify_signature(&quote)?;

        let tcb_expiration = CollateralVerifier::verify_signed_collateral(
            &input.collateral.tcb_info,
            TdxSignedCollateralBody::TcbInfo,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )?;
        let qe_expiration = CollateralVerifier::verify_signed_collateral(
            &input.collateral.qe_identity,
            TdxSignedCollateralBody::QeIdentity,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )?;
        let pck_leaf =
            input.pck_certificate_chain.last().expect("verified certificate chain is non-empty");
        let (pck_platform, pck_tcb) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)?;
        let tcb_info_document = input.collateral.tcb_info.tcb_info_document()?;
        tcb_info_document.tcb_info.verify_platform(&pck_platform)?;
        let qe_identity_document = input.collateral.qe_identity.qe_identity_document()?;
        qe_identity_document.enclave_identity.verify_qe_report(&quote)?;

        let tcb_status =
            tcb_info_document.tcb_info.tcb_status_for_quote(&quote, &pck_tcb)?.to_contract_status();
        if tcb_status == TDXTcbStatus::Unknown || !input.allowed_tcb_statuses.contains(&tcb_status)
        {
            return Err(TdxVerifierError::TcbStatusNotAllowed);
        }

        let timestamp_seconds = input.quote_timestamp_millis / 1_000;
        if timestamp_seconds >= input.verification_time
            || input.verification_time - timestamp_seconds >= input.max_quote_age_seconds
        {
            return Err(TdxVerifierError::InvalidTimestamp);
        }

        let public_key_hash = Self::validate_public_key(&input.expected_public_key)?;
        let signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        Self::verify_report_data(
            &quote,
            public_key_hash,
            input.quote_timestamp_millis,
            input.attestation_nonce,
        )?;
        let collateral_expiration = pck_expiration.min(tcb_expiration).min(qe_expiration);

        Ok(TDXVerifierJournal {
            result: TDXVerificationResult::Success,
            tcbStatus: tcb_status,
            timestamp: input.quote_timestamp_millis,
            collateralExpiration: collateral_expiration,
            rootCaHash: input.trusted_root_ca_hash,
            pckCertHash: pck_leaf.hash(),
            tcbInfoHash: input.collateral.tcb_info.hash(),
            qeIdentityHash: input.collateral.qe_identity.hash(),
            publicKey: input.expected_public_key.clone(),
            signer,
            imageHash: quote.image_hash(),
            mrTdHash: keccak256(quote.mrtd),
            reportDataPrefix: quote.report_data_prefix(),
            reportDataSuffix: quote.report_data_suffix(),
        })
    }

    /// Validates and hashes an uncompressed secp256k1 signer public key.
    pub fn validate_public_key(public_key: &[u8]) -> Result<B256> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxVerifierError::MalformedPublicKey);
        }
        PublicKey::from_sec1_bytes(public_key).map_err(|_| TdxVerifierError::MalformedPublicKey)?;
        Ok(keccak256(&public_key[1..]))
    }

    /// Validates that quote report data binds the signer public key, quote
    /// timestamp, and optional registrar nonce.
    pub fn verify_report_data(
        quote: &crate::ParsedTdxQuote,
        public_key_hash: B256,
        timestamp_millis: u64,
        attestation_nonce: Option<B256>,
    ) -> Result<()> {
        if quote.report_data_prefix() != public_key_hash
            || quote.report_data_suffix()
                != Self::timestamp_report_data_suffix(timestamp_millis, attestation_nonce)
        {
            return Err(TdxVerifierError::ReportDataMismatch);
        }
        Ok(())
    }

    /// Computes the expected signed `TDREPORT.REPORTDATA` suffix for a quote
    /// timestamp and optional registrar nonce.
    pub fn timestamp_report_data_suffix(
        timestamp_millis: u64,
        attestation_nonce: Option<B256>,
    ) -> B256 {
        let mut preimage = [&REPORT_DATA_CONTEXT[..], &timestamp_millis.to_le_bytes()].concat();
        if let Some(nonce) = attestation_nonce {
            preimage.extend_from_slice(nonce.as_slice());
        }
        keccak256(preimage)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, address, hex};
    use k256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use serde_json::json;

    use super::*;
    use crate::{
        TdxCertificate, TdxCollateral, TdxRevocationEvidence, TdxSignedCollateral,
        TdxSignedCollateralBody::{QeIdentity, TcbInfo},
        quote::{
            CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
            ECDSA_P256_SIGNATURE_LEN, QE_REPORT_LEN, TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN,
            TDX_REPORT_BODY_LEN, TDX_TEE_TCB_SVN_LEN,
        },
    };

    const VERIFICATION_TIME: u64 = 1_711_111_111;
    const QUOTE_TIMESTAMP_MILLIS: u64 = 1_711_111_000_000;
    const MAX_QUOTE_AGE_SECONDS: u64 = 300;
    const FIXTURE_HEX: &str = include_str!("testdata/verify_fixture.hex");

    fn secp256k1_public_key(scalar: u8) -> Bytes {
        let mut bytes = [0; 32];
        bytes[31] = scalar;
        let secret_key = SecretKey::from_slice(&bytes).expect("fixture secret key must be valid");
        Bytes::copy_from_slice(secret_key.public_key().to_encoded_point(false).as_bytes())
    }

    fn sign(message: &[u8]) -> Bytes {
        let signature: Signature = SigningKey::from_slice(&[4; 32])
            .expect("fixture signing key must be valid")
            .sign(message);
        Bytes::copy_from_slice(&signature.to_bytes())
    }

    fn fixture_bytes(name: &str) -> Bytes {
        FIXTURE_HEX
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| {
                (key == name)
                    .then(|| Bytes::from(hex::decode(value).expect("static hex fixture decodes")))
            })
            .unwrap_or_else(|| panic!("missing static hex fixture {name}"))
    }

    fn fixture_cert(name: &str) -> TdxCertificate {
        TdxCertificate { raw: fixture_bytes(name) }
    }

    fn revocation_evidence(intermediate_crl: &str) -> TdxRevocationEvidence {
        TdxRevocationEvidence {
            certificate_crls: vec![fixture_bytes("root_crl"), fixture_bytes(intermediate_crl)],
        }
    }

    fn collateral(
        raw: Bytes,
        body_kind: TdxSignedCollateralBody,
        signing_chain: Vec<TdxCertificate>,
    ) -> TdxSignedCollateral {
        let mut collateral = TdxSignedCollateral { raw, signing_chain, signature: Bytes::new() };
        resign_collateral_body(&mut collateral, body_kind);
        collateral
    }

    fn resign_collateral_body(
        collateral: &mut TdxSignedCollateral,
        body_kind: TdxSignedCollateralBody,
    ) {
        let signed_body = collateral
            .signed_body_bytes(body_kind)
            .expect("fixture collateral body must serialize");
        collateral.signature = sign(&signed_body);
    }

    fn fixture() -> TdxVerifierInput {
        let root = fixture_cert("root");
        let intermediate = fixture_cert("intermediate");
        let pck_leaf = fixture_cert("pck_leaf");
        let collateral_leaf = fixture_cert("collateral_leaf");

        let root_hash = root.hash();
        let pck_chain = vec![root.clone(), intermediate.clone(), pck_leaf];
        let collateral_chain = vec![root, intermediate, collateral_leaf];
        let tcb_info = collateral(fixture_bytes("tcb_info_raw"), TcbInfo, collateral_chain.clone());
        let qe_identity =
            collateral(fixture_bytes("qe_identity_raw"), QeIdentity, collateral_chain);
        TdxVerifierInput {
            quote: fixture_bytes("quote"),
            pck_certificate_chain: pck_chain,
            collateral: TdxCollateral { tcb_info, qe_identity },
            revocation: revocation_evidence("intermediate_crl"),
            trusted_root_ca_hash: root_hash,
            expected_public_key: secp256k1_public_key(1),
            attestation_nonce: None,
            quote_timestamp_millis: QUOTE_TIMESTAMP_MILLIS,
            verification_time: VERIFICATION_TIME,
            max_quote_age_seconds: MAX_QUOTE_AGE_SECONDS,
            allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate],
        }
    }

    fn edit_quote(input: &mut TdxVerifierInput, mutate: impl FnOnce(&mut [u8])) {
        let mut quote = input.quote.to_vec();
        mutate(&mut quote);
        input.quote = Bytes::from(quote);
    }

    fn edit_collateral(
        input: &mut TdxVerifierInput,
        body_kind: TdxSignedCollateralBody,
        mutate: impl FnOnce(&mut serde_json::Value),
    ) {
        let collateral = match body_kind {
            TcbInfo => &mut input.collateral.tcb_info,
            QeIdentity => &mut input.collateral.qe_identity,
        };
        let mut document =
            serde_json::from_slice(&collateral.raw).expect("fixture JSON must parse");
        mutate(&mut document);
        collateral.raw =
            Bytes::from(serde_json::to_vec(&document).expect("fixture JSON must serialize"));
        resign_collateral_body(collateral, body_kind);
    }

    #[test]
    fn verifies_known_good_tdx_quote_fixture_and_emits_solidity_journal() {
        let input = fixture();
        let journal = TdxVerifier::verify(&input).unwrap();

        assert_eq!(journal.result, TDXVerificationResult::Success);
        assert_eq!(journal.tcbStatus, TDXTcbStatus::UpToDate);
        assert_eq!(journal.timestamp, QUOTE_TIMESTAMP_MILLIS);
        assert_eq!(journal.rootCaHash, input.trusted_root_ca_hash);
        assert_eq!(journal.pckCertHash, input.pck_certificate_chain[2].hash());
        assert_eq!(journal.tcbInfoHash, input.collateral.tcb_info.hash());
        assert_eq!(journal.qeIdentityHash, input.collateral.qe_identity.hash());
        assert_eq!(journal.publicKey, input.expected_public_key);
        assert_eq!(journal.signer, address!("7e5f4552091a69125d5dfcb7b8c2659029395bdf"));
        assert_eq!(
            journal.reportDataSuffix,
            TdxVerifier::timestamp_report_data_suffix(QUOTE_TIMESTAMP_MILLIS, None)
        );
        assert_eq!(
            journal.collateralExpiration, 2_051_222_400,
            "earliest collateral/cert expiration must be journaled",
        );
    }

    #[test]
    fn verifies_tdx_tcb_info_without_tee_type() {
        let mut input = fixture();
        edit_collateral(&mut input, TcbInfo, |document| {
            document["tcbInfo"].as_object_mut().unwrap().remove("teeType");
        });

        TdxVerifier::verify(&input).unwrap();
    }

    #[test]
    fn collateral_signature_covers_signed_json_body() {
        let mut input = fixture();
        let document: serde_json::Value =
            serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
        input.collateral.tcb_info.raw = Bytes::from(serde_json::to_vec_pretty(&document).unwrap());
        resign_collateral_body(&mut input.collateral.tcb_info, TcbInfo);

        TdxVerifier::verify(&input).expect("body-signed pretty collateral must verify");

        input.collateral.tcb_info.signature = sign(&input.collateral.tcb_info.raw);
        let error =
            TdxVerifier::verify(&input).expect_err("top-level collateral signature must fail");
        assert!(matches!(error, TdxVerifierError::TcbInfoInvalid(_)), "{error:?}");
    }

    #[test]
    fn qe_identity_signature_must_not_be_bound_to_tcb_info_body() {
        let mut input = fixture();
        let tcb_document: serde_json::Value =
            serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
        let qe_document: serde_json::Value =
            serde_json::from_slice(&input.collateral.qe_identity.raw).unwrap();
        input.collateral.qe_identity.raw = Bytes::from(
            serde_json::to_vec(&json!({
                "tcbInfo": tcb_document["tcbInfo"].clone(),
                "enclaveIdentity": qe_document["enclaveIdentity"].clone(),
            }))
            .expect("fixture JSON must serialize"),
        );

        let signed_tcb_body = input
            .collateral
            .tcb_info
            .signed_body_bytes(TcbInfo)
            .expect("fixture collateral body must serialize");
        input.collateral.qe_identity.signature = sign(&signed_tcb_body);

        let error = TdxVerifier::verify(&input)
            .expect_err("QE identity collateral with multiple signed bodies must fail");
        assert!(matches!(error, TdxVerifierError::QeIdentityInvalid(_)), "{error:?}");
    }

    #[test]
    fn malformed_signer_public_key_must_be_on_secp256k1_curve() {
        let mut public_key = vec![0x04];
        public_key.extend_from_slice(&[0; 64]);

        let error =
            TdxVerifier::validate_public_key(&public_key).expect_err("off-curve key must fail");
        assert!(matches!(error, TdxVerifierError::MalformedPublicKey));
    }

    #[test]
    fn quote_timestamp_must_match_signed_report_data() {
        let mut input = fixture();
        input.quote_timestamp_millis = (VERIFICATION_TIME - 1) * 1_000;

        let error = TdxVerifier::verify(&input)
            .expect_err("fresh input timestamp must not replay an older signed quote");

        assert!(matches!(error, TdxVerifierError::ReportDataMismatch));
    }

    #[test]
    fn registrar_nonce_must_match_signed_report_data() {
        let mut input = fixture();
        input.attestation_nonce = Some(B256::repeat_byte(0x11));

        let error = TdxVerifier::verify(&input)
            .expect_err("quote without the expected registrar nonce must fail");

        assert!(matches!(error, TdxVerifierError::ReportDataMismatch));
    }

    #[test]
    fn collateral_expiration_includes_earliest_crl_next_update() {
        let mut input = fixture();
        input.revocation = TdxRevocationEvidence {
            certificate_crls: vec![
                fixture_bytes("root_crl_early"),
                fixture_bytes("intermediate_crl_early"),
            ],
        };

        let journal = TdxVerifier::verify(&input).unwrap();

        assert_eq!(journal.collateralExpiration, 1_893_456_000);
    }

    macro_rules! assert_failure {
        (|$input:ident| $mutate:expr, $pattern:pat) => {{
            let mut verifier_input = fixture();
            let $input = &mut verifier_input;
            $mutate;
            assert!(matches!(
                TdxVerifier::verify(&verifier_input).expect_err("verification must fail"),
                $pattern
            ));
        }};
    }

    #[test]
    fn failure_cases_return_expected_error() {
        assert_failure!(
            |input| {
                let signature_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
                edit_quote(input, |quote| quote[signature_offset] ^= 0x01);
            },
            TdxVerifierError::QuoteSignatureInvalid(_)
        );
        assert_failure!(
            |input| edit_quote(input, |quote| quote[4..8].copy_from_slice(&0u32.to_le_bytes())),
            TdxVerifierError::InvalidQuote(_)
        );
        assert_failure!(
            |input| edit_quote(input, |quote| quote[2..4].copy_from_slice(&1u16.to_le_bytes())),
            TdxVerifierError::InvalidQuote(_)
        );
        assert_failure!(
            |input| {
                let signature_data_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
                let qe_report_signature_offset = signature_data_offset
                    + ECDSA_P256_SIGNATURE_LEN
                    + ECDSA_P256_PUBLIC_KEY_BODY_LEN
                    + CERTIFICATION_DATA_HEADER_LEN
                    + QE_REPORT_LEN;
                edit_quote(input, |quote| quote[qe_report_signature_offset] ^= 0x01);
            },
            TdxVerifierError::PckCertChainInvalid(_)
        );
        assert_failure!(
            |input| input.trusted_root_ca_hash = B256::repeat_byte(0xEF),
            TdxVerifierError::RootCaNotTrusted
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["nextUpdate"] = json!("2024-03-01T00:00:00Z");
            }),
            TdxVerifierError::CollateralExpired
        );
        assert_failure!(
            |input| input.revocation = revocation_evidence("intermediate_crl_revoked_04"),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| input.verification_time = VERIFICATION_TIME + MAX_QUOTE_AGE_SECONDS + 1,
            TdxVerifierError::InvalidTimestamp
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["tcbLevels"][0]["tcbStatus"] = json!("Revoked");
            }),
            TdxVerifierError::TcbStatusNotAllowed
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["id"] = json!("SGX");
                document["tcbInfo"]["teeType"] = json!("00000000");
            }),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| input.collateral.tcb_info.signature = Bytes::from(vec![0]),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| input.collateral.qe_identity.signature = Bytes::from(vec![0]),
            TdxVerifierError::QeIdentityInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, QeIdentity, |document| {
                document["enclaveIdentity"]["tcbLevels"][0]["tcbStatus"] = json!("Revoked");
            }),
            TdxVerifierError::QeIdentityInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, QeIdentity, |document| {
                document["enclaveIdentity"]["id"] = json!("QE");
            }),
            TdxVerifierError::QeIdentityInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, QeIdentity, |document| {
                document["enclaveIdentity"]["id"] = json!("QVE");
            }),
            TdxVerifierError::QeIdentityInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, QeIdentity, |document| {
                document["enclaveIdentity"]["version"] = json!(1);
            }),
            TdxVerifierError::QeIdentityInvalid(_)
        );
        assert_failure!(
            |input| input.expected_public_key = Bytes::from(vec![0x04; 64]),
            TdxVerifierError::MalformedPublicKey
        );
        assert_failure!(
            |input| input.expected_public_key = secp256k1_public_key(2),
            TdxVerifierError::ReportDataMismatch
        );
        assert_failure!(
            |input| input.revocation = revocation_evidence("intermediate_crl_revoked_03"),
            TdxVerifierError::PckCertChainInvalid(_)
        );
        assert_failure!(
            |input| {
                input.pck_certificate_chain[2] = fixture_cert("pck_leaf_downgraded_tcb");
                edit_collateral(input, TcbInfo, |document| {
                    document["tcbInfo"]["tcbLevels"] = json!([{
                        "tcb": {
                            "pcesvn": 8,
                            "sgxtcbcomponents": vec![json!({ "svn": 2 }); TDX_TEE_TCB_SVN_LEN],
                            "tdxtcbcomponents": vec![json!({ "svn": 3 }); TDX_TEE_TCB_SVN_LEN],
                        },
                        "tcbStatus": "OutOfDate",
                    }]);
                });
            },
            TdxVerifierError::TcbStatusNotAllowed
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["tdxModuleIdentities"][0]["tcbLevels"][0]["tcbStatus"] =
                    json!("OutOfDate");
            }),
            TdxVerifierError::TcbStatusNotAllowed
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["tdxModuleIdentities"][0]["id"] = json!("TDX_04");
            }),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["tdxModuleIdentities"][0]["mrsigner"] =
                    json!("11".repeat(TDX_MEASUREMENT_LEN));
            }),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| {
                input.pck_certificate_chain[2] = fixture_cert("pck_leaf_serial_80");
                input.revocation = revocation_evidence("intermediate_crl_revoked_80");
            },
            TdxVerifierError::PckCertChainInvalid(_)
        );
        assert_failure!(
            |input| {
                input.collateral.tcb_info.signing_chain[2] =
                    fixture_cert("collateral_leaf_wrong_subject");
            },
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| {
                input.collateral.tcb_info.signing_chain[2] =
                    fixture_cert("collateral_leaf_key_usage_20");
            },
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| input.revocation = TdxRevocationEvidence::default(),
            TdxVerifierError::PckCertChainInvalid(_)
        );
        assert_failure!(
            |input| edit_collateral(input, TcbInfo, |document| {
                document["tcbInfo"]["fmspc"] = json!("060504030201");
            }),
            TdxVerifierError::TcbInfoInvalid(_)
        );
        assert_failure!(
            |input| {
                let mut signature = input.collateral.qe_identity.signature.to_vec();
                signature[0] ^= 0x01;
                input.collateral.qe_identity.signature = Bytes::from(signature);
            },
            TdxVerifierError::QeIdentityInvalid(_)
        );
    }
}
