//! End-to-end TDX quote, collateral, policy, and journal verification.

use alloy_primitives::{Address, B256, Bytes, keccak256};
use k256::PublicKey;

use crate::{
    ParsedTdxQuote, Result, TDXTcbStatus, TDXVerificationResult, TDXVerifierJournal,
    TdxCertificate, TdxCollateral, TdxPlatformIdentity, TdxQuote, TdxRevocationEvidence,
    TdxSignedCollateralBody, TdxVerifierError, collateral::CollateralVerifier,
};

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

        let (collateral_expiration, tcb_status) = Self::verify_quote_collateral(
            &quote,
            &input.pck_certificate_chain,
            &input.collateral,
            &input.revocation,
            input.trusted_root_ca_hash,
            input.verification_time,
        )?;
        if tcb_status == TDXTcbStatus::Unknown || !input.allowed_tcb_statuses.contains(&tcb_status)
        {
            return Err(TdxVerifierError::TcbStatusNotAllowed);
        }

        Self::verify_quote_timestamp(
            input.quote_timestamp_millis,
            input.verification_time,
            input.max_quote_age_seconds,
        )?;

        let public_key_hash = Self::validate_public_key(&input.expected_public_key)?;
        let signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        Self::verify_report_data(&quote, public_key_hash, input.quote_timestamp_millis)?;
        let pck_leaf =
            input.pck_certificate_chain.last().expect("verified certificate chain is non-empty");

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

    /// Verifies quote-bound collateral and returns expiration plus contract TCB status.
    pub fn verify_quote_collateral(
        quote: &ParsedTdxQuote,
        pck_certificate_chain: &[TdxCertificate],
        collateral: &TdxCollateral,
        revocation: &TdxRevocationEvidence,
        trusted_root_ca_hash: B256,
        verification_time: u64,
    ) -> Result<(u64, TDXTcbStatus)> {
        let (pck_leaf_key, pck_expiration) = CollateralVerifier::verify_certificate_chain(
            pck_certificate_chain,
            trusted_root_ca_hash,
            verification_time,
            revocation,
        )
        .map_err(|e| {
            if matches!(e, TdxVerifierError::RootCaNotTrusted) {
                e
            } else {
                TdxVerifierError::PckCertChainInvalid(e.to_string())
            }
        })?;

        TdxQuote::verify_qe_report(quote, &pck_leaf_key)?;
        TdxQuote::verify_signature(quote)?;

        let tcb_expiration = CollateralVerifier::verify_signed_collateral(
            &collateral.tcb_info,
            TdxSignedCollateralBody::TcbInfo,
            trusted_root_ca_hash,
            verification_time,
            revocation,
        )?;
        let qe_expiration = CollateralVerifier::verify_signed_collateral(
            &collateral.qe_identity,
            TdxSignedCollateralBody::QeIdentity,
            trusted_root_ca_hash,
            verification_time,
            revocation,
        )?;

        let pck_leaf =
            pck_certificate_chain.last().expect("verified certificate chain is non-empty");
        let (pck_platform, pck_tcb) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)?;
        let tcb_info_document = collateral.tcb_info.tcb_info_document()?;
        tcb_info_document.tcb_info.verify_platform(&pck_platform)?;
        let qe_identity_document = collateral.qe_identity.qe_identity_document()?;
        qe_identity_document.enclave_identity.verify_qe_report(quote)?;

        let tcb_status =
            tcb_info_document.tcb_info.tcb_status_for_quote(quote, &pck_tcb)?.to_contract_status();
        let collateral_expiration = pck_expiration.min(tcb_expiration).min(qe_expiration);

        Ok((collateral_expiration, tcb_status))
    }

    /// Validates and hashes an uncompressed secp256k1 signer public key.
    pub fn validate_public_key(public_key: &[u8]) -> Result<B256> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxVerifierError::MalformedPublicKey);
        }
        PublicKey::from_sec1_bytes(public_key).map_err(|_| TdxVerifierError::MalformedPublicKey)?;
        Ok(keccak256(&public_key[1..65]))
    }

    /// Computes the expected signed `TDREPORT.REPORTDATA` suffix for a quote timestamp.
    pub fn timestamp_report_data_suffix(timestamp_millis: u64) -> B256 {
        keccak256([&b"base-tdx-tee-prover-v1"[..], &timestamp_millis.to_le_bytes()[..]].concat())
    }

    /// Verifies that `TDREPORT.REPORTDATA` binds both the signer key and quote timestamp.
    pub fn verify_report_data(
        quote: &crate::ParsedTdxQuote,
        public_key_hash: B256,
        timestamp_millis: u64,
    ) -> Result<()> {
        if quote.report_data_prefix() != public_key_hash
            || quote.report_data_suffix() != Self::timestamp_report_data_suffix(timestamp_millis)
        {
            return Err(TdxVerifierError::ReportDataMismatch);
        }
        Ok(())
    }

    /// Verifies quote timestamp age and future-skew policy.
    pub fn verify_quote_timestamp(
        timestamp_millis: u64,
        verification_time_seconds: u64,
        max_quote_age_seconds: u64,
    ) -> Result<()> {
        let timestamp_seconds = timestamp_millis / 1_000;
        if timestamp_seconds
            .checked_add(max_quote_age_seconds)
            .is_none_or(|expiry| expiry <= verification_time_seconds)
            || timestamp_seconds >= verification_time_seconds
        {
            return Err(TdxVerifierError::InvalidTimestamp);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, hex, keccak256};
    use alloy_sol_types::SolValue;
    use k256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use serde_json::json;

    use super::*;
    use crate::{
        TdxCertificate, TdxCollateral, TdxRevocationEvidence, TdxSignedCollateral,
        collateral::{TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION},
        quote::{
            CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
            ECDSA_P256_SIGNATURE_LEN, QE_REPORT_LEN, TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN,
            TDX_REPORT_BODY_LEN, TDX_SEAM_ATTRIBUTES_LEN, TDX_TEE_TCB_SVN_LEN, TDX_TEE_TYPE,
        },
    };

    const VERIFICATION_TIME: u64 = 1_711_111_111;
    const QUOTE_TIMESTAMP_MILLIS: u64 = 1_711_111_000_000;
    const MAX_QUOTE_AGE_SECONDS: u64 = 300;
    const COLLATERAL_NEXT_UPDATE: u64 = 2_051_222_400;
    const EARLY_CRL_NEXT_UPDATE: u64 = 1_893_456_000;
    const COLLATERAL_ISSUE_DATE: &str = "2024-01-01T00:00:00Z";
    const COLLATERAL_NEXT_UPDATE_DATE: &str = "2035-01-01T00:00:00Z";
    const EXPIRED_COLLATERAL_NEXT_UPDATE_DATE: &str = "2024-03-01T00:00:00Z";
    const FMSPC_HEX: &str = "010203040506";
    const PCE_ID_HEX: &str = "0009";
    const FIXTURE_HEX: &str = include_str!("testdata/verify_fixture.hex");

    fn signing_key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).expect("fixture signing key must be valid")
    }

    fn secp256k1_public_key(scalar: u8) -> Bytes {
        let mut bytes = [0; 32];
        bytes[31] = scalar;
        let secret_key = SecretKey::from_slice(&bytes).expect("fixture secret key must be valid");
        Bytes::copy_from_slice(secret_key.public_key().to_encoded_point(false).as_bytes())
    }

    fn signer_address(public_key: &[u8]) -> Address {
        let public_key_hash = TdxVerifier::validate_public_key(public_key).unwrap();
        Address::from_slice(&public_key_hash.as_slice()[12..])
    }

    fn sign(key: &SigningKey, message: &[u8]) -> Bytes {
        let signature: Signature = key.sign(message);
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

    fn signed_body_bytes(raw: &[u8], body_kind: TdxSignedCollateralBody) -> Vec<u8> {
        TdxSignedCollateral {
            raw: Bytes::copy_from_slice(raw),
            signing_chain: Vec::new(),
            signature: Bytes::new(),
        }
        .signed_body_bytes(body_kind)
        .expect("fixture collateral body must serialize")
    }

    fn collateral(
        raw: &[u8],
        body_kind: TdxSignedCollateralBody,
        signing_key: &SigningKey,
        signing_chain: Vec<TdxCertificate>,
    ) -> TdxSignedCollateral {
        let signed_body = signed_body_bytes(raw, body_kind);
        TdxSignedCollateral {
            raw: Bytes::copy_from_slice(raw),
            signing_chain,
            signature: sign(signing_key, &signed_body),
        }
    }

    fn resign_collateral_body(
        collateral: &mut TdxSignedCollateral,
        body_kind: TdxSignedCollateralBody,
        signing_key: &SigningKey,
    ) {
        let signed_body = collateral
            .signed_body_bytes(body_kind)
            .expect("fixture collateral body must serialize");
        collateral.signature = sign(signing_key, &signed_body);
    }

    fn resign_tcb_info(input: &mut TdxVerifierInput) {
        resign_collateral_body(
            &mut input.collateral.tcb_info,
            TdxSignedCollateralBody::TcbInfo,
            &signing_key(4),
        );
    }

    fn resign_qe_identity(input: &mut TdxVerifierInput) {
        resign_collateral_body(
            &mut input.collateral.qe_identity,
            TdxSignedCollateralBody::QeIdentity,
            &signing_key(4),
        );
    }

    fn json_bytes(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("fixture JSON must serialize")
    }

    fn tcb_components(svn: u16) -> Vec<serde_json::Value> {
        (0..TDX_TEE_TCB_SVN_LEN).map(|_| json!({ "svn": svn })).collect()
    }

    fn tcb_level(status: &str, sgx_svn: u16, tdx_svn: u16, pce_svn: u16) -> serde_json::Value {
        json!({
            "tcb": {
                "pcesvn": pce_svn,
                "sgxtcbcomponents": tcb_components(sgx_svn),
                "tdxtcbcomponents": tcb_components(tdx_svn),
            },
            "tcbStatus": status,
        })
    }

    fn tcb_info_raw_with_levels_and_module_status(
        levels: &[serde_json::Value],
        next_update: &str,
        module_status: &str,
        module_isvsvn: u16,
    ) -> Vec<u8> {
        json_bytes(json!({
            "tcbInfo": {
                "id": "TDX",
                "teeType": format!("{TDX_TEE_TYPE:08x}"),
                "issueDate": COLLATERAL_ISSUE_DATE,
                "nextUpdate": next_update,
                "fmspc": FMSPC_HEX,
                "pceId": PCE_ID_HEX,
                "tdxModule": {
                    "mrsigner": "00".repeat(TDX_MEASUREMENT_LEN),
                    "attributes": "00".repeat(TDX_SEAM_ATTRIBUTES_LEN),
                    "attributesMask": "ff".repeat(TDX_SEAM_ATTRIBUTES_LEN),
                },
                "tdxModuleIdentities": [{
                    "id": "TDX_03",
                    "mrsigner": "00".repeat(TDX_MEASUREMENT_LEN),
                    "attributes": "00".repeat(TDX_SEAM_ATTRIBUTES_LEN),
                    "attributesMask": "ff".repeat(TDX_SEAM_ATTRIBUTES_LEN),
                    "tcbLevels": [{ "tcb": { "isvsvn": module_isvsvn }, "tcbStatus": module_status }],
                }],
                "tcbLevels": levels,
            }
        }))
    }

    fn tcb_info_raw_with_levels(levels: &[serde_json::Value], next_update: &str) -> Vec<u8> {
        tcb_info_raw_with_levels_and_module_status(levels, next_update, "UpToDate", 3)
    }

    fn tcb_info_raw_with_dates(status: &str, next_update: &str) -> Vec<u8> {
        tcb_info_raw_with_levels(&[tcb_level(status, 3, 3, 9)], next_update)
    }

    fn tcb_info_raw(status: &str) -> Vec<u8> {
        tcb_info_raw_with_dates(status, COLLATERAL_NEXT_UPDATE_DATE)
    }

    fn sgx_tcb_info_raw(status: &str) -> Vec<u8> {
        let components = tcb_components(3);
        json_bytes(json!({
            "tcbInfo": {
                "id": "SGX",
                "teeType": "00000000",
                "issueDate": COLLATERAL_ISSUE_DATE,
                "nextUpdate": COLLATERAL_NEXT_UPDATE_DATE,
                "fmspc": FMSPC_HEX,
                "pceId": PCE_ID_HEX,
                "tcbLevels": [{ "tcb": { "pcesvn": 9, "sgxtcbcomponents": components }, "tcbStatus": status }],
            }
        }))
    }

    fn qe_identity_raw_with_identity(id: &str, version: u16, status: &str) -> Vec<u8> {
        json_bytes(json!({
            "enclaveIdentity": {
                "id": id,
                "version": version,
                "issueDate": COLLATERAL_ISSUE_DATE,
                "nextUpdate": COLLATERAL_NEXT_UPDATE_DATE,
                "miscselect": "00000000",
                "miscselectMask": "ffffffff",
                "attributes": "00000000000000000000000000000000",
                "attributesMask": "ffffffffffffffffffffffffffffffff",
                "mrsigner": "0000000000000000000000000000000000000000000000000000000000000000",
                "isvprodid": 0,
                "tcbLevels": [{ "tcb": { "isvsvn": 0 }, "tcbStatus": status }],
            }
        }))
    }

    fn qe_identity_raw_with_status(status: &str) -> Vec<u8> {
        qe_identity_raw_with_identity(TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION, status)
    }

    fn fixture() -> TdxVerifierInput {
        let collateral_key = signing_key(4);
        let root = fixture_cert("root");
        let intermediate = fixture_cert("intermediate");
        let pck_leaf = fixture_cert("pck_leaf");
        let collateral_leaf = fixture_cert("collateral_leaf");

        let root_hash = root.hash();
        let pck_chain = vec![root.clone(), intermediate.clone(), pck_leaf];
        let collateral_chain = vec![root, intermediate, collateral_leaf];
        let tcb_info = collateral(
            &tcb_info_raw("UpToDate"),
            TdxSignedCollateralBody::TcbInfo,
            &collateral_key,
            collateral_chain.clone(),
        );
        let qe_identity = collateral(
            &qe_identity_raw_with_status("UpToDate"),
            TdxSignedCollateralBody::QeIdentity,
            &collateral_key,
            collateral_chain,
        );
        TdxVerifierInput {
            quote: fixture_bytes("quote"),
            pck_certificate_chain: pck_chain,
            collateral: TdxCollateral { tcb_info, qe_identity },
            revocation: revocation_evidence("intermediate_crl"),
            trusted_root_ca_hash: root_hash,
            expected_public_key: secp256k1_public_key(1),
            quote_timestamp_millis: QUOTE_TIMESTAMP_MILLIS,
            verification_time: VERIFICATION_TIME,
            max_quote_age_seconds: MAX_QUOTE_AGE_SECONDS,
            allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate],
        }
    }

    #[test]
    fn verifies_known_good_tdx_quote_fixture_and_emits_solidity_journal() {
        let input = fixture();
        let journal = TdxVerifier::verify(&input).unwrap();

        assert_eq!(journal.result as u8, TDXVerificationResult::Success as u8);
        assert_eq!(journal.tcbStatus as u8, TDXTcbStatus::UpToDate as u8);
        assert_eq!(journal.timestamp, QUOTE_TIMESTAMP_MILLIS);
        assert_eq!(journal.rootCaHash, input.trusted_root_ca_hash);
        assert_eq!(journal.pckCertHash, input.pck_certificate_chain[2].hash());
        assert_eq!(journal.tcbInfoHash, input.collateral.tcb_info.hash());
        assert_eq!(journal.qeIdentityHash, input.collateral.qe_identity.hash());
        assert_eq!(journal.publicKey, input.expected_public_key);
        assert_eq!(journal.signer, signer_address(&input.expected_public_key));
        assert_eq!(
            journal.reportDataSuffix,
            TdxVerifier::timestamp_report_data_suffix(QUOTE_TIMESTAMP_MILLIS)
        );
        assert_eq!(
            journal.collateralExpiration, COLLATERAL_NEXT_UPDATE,
            "earliest collateral/cert expiration must be journaled",
        );

        let encoded = SolValue::abi_encode(&journal);
        let decoded = <TDXVerifierJournal as SolValue>::abi_decode_validate(&encoded)
            .expect("journal must decode with Solidity ABI type");
        assert_eq!(decoded.result as u8, TDXVerificationResult::Success as u8);
        assert_eq!(decoded.signer, signer_address(&input.expected_public_key));
        assert_eq!(decoded.imageHash, journal.imageHash);
        assert_eq!(decoded.mrTdHash, journal.mrTdHash);
        assert_eq!(decoded.reportDataPrefix, journal.reportDataPrefix);
        assert_eq!(decoded.reportDataSuffix, journal.reportDataSuffix);
    }

    #[test]
    fn verifies_tdx_tcb_info_without_tee_type() {
        let mut input = fixture();
        let raw = String::from_utf8(tcb_info_raw("UpToDate"))
            .unwrap()
            .replace(r#""teeType":"00000081","#, "");
        input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
        resign_tcb_info(&mut input);

        TdxVerifier::verify(&input).unwrap();
    }

    #[test]
    fn collateral_signature_covers_signed_json_body() {
        let mut input = fixture();
        let document: serde_json::Value =
            serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
        input.collateral.tcb_info.raw =
            Bytes::from(serde_json::to_string_pretty(&document).unwrap().into_bytes());
        resign_tcb_info(&mut input);

        TdxVerifier::verify(&input).expect("body-signed pretty collateral must verify");

        input.collateral.tcb_info.signature = sign(&signing_key(4), &input.collateral.tcb_info.raw);
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
        let mut combined_document = serde_json::Map::new();
        combined_document.insert("tcbInfo".into(), tcb_document["tcbInfo"].clone());
        combined_document.insert("enclaveIdentity".into(), qe_document["enclaveIdentity"].clone());
        input.collateral.qe_identity.raw =
            Bytes::from(serde_json::to_vec(&combined_document).unwrap());

        let signed_tcb_body =
            signed_body_bytes(&input.collateral.tcb_info.raw, TdxSignedCollateralBody::TcbInfo);
        input.collateral.qe_identity.signature = sign(&signing_key(4), &signed_tcb_body);

        let error = TdxVerifier::verify(&input)
            .expect_err("QE identity collateral with multiple signed bodies must fail");
        assert!(matches!(error, TdxVerifierError::QeIdentityInvalid(_)), "{error:?}");
    }

    #[test]
    fn image_hash_matches_contract_formula() {
        let parsed = TdxQuote::parse(&fixture().quote).unwrap();
        let expected = [
            &parsed.mrtd[..],
            &parsed.rtmr0[..],
            &parsed.rtmr1[..],
            &parsed.rtmr2[..],
            &parsed.rtmr3[..],
        ]
        .concat();
        assert_eq!(parsed.image_hash(), keccak256(expected));
        assert_eq!(keccak256(parsed.mrtd), TdxVerifier::verify(&fixture()).unwrap().mrTdHash);
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
    fn quote_v5_is_rejected_until_body_layout_is_supported() {
        let mut quote = fixture().quote.to_vec();
        quote[0..2].copy_from_slice(&5u16.to_le_bytes());

        let error = TdxQuote::parse(&quote).expect_err("quote v5 must not use v4 body offsets");

        assert!(
            matches!(error, TdxVerifierError::InvalidQuote(message) if message == "unsupported quote version 5")
        );
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
    fn quote_timestamp_allows_strictly_past_and_inside_max_age() {
        for timestamp_millis in [
            (VERIFICATION_TIME - 1) * 1_000,
            (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS + 1) * 1_000,
        ] {
            TdxVerifier::verify_quote_timestamp(
                timestamp_millis,
                VERIFICATION_TIME,
                MAX_QUOTE_AGE_SECONDS,
            )
            .expect("quote inside timestamp policy must be accepted");
        }
    }

    #[test]
    fn quote_timestamp_rejects_contract_boundaries_future_second_and_over_age() {
        for timestamp_millis in [
            VERIFICATION_TIME * 1_000,
            (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS) * 1_000,
            (VERIFICATION_TIME + 1) * 1_000,
            (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS - 1) * 1_000,
        ] {
            let error = TdxVerifier::verify_quote_timestamp(
                timestamp_millis,
                VERIFICATION_TIME,
                MAX_QUOTE_AGE_SECONDS,
            )
            .expect_err("quote outside timestamp policy must fail");
            assert!(matches!(error, TdxVerifierError::InvalidTimestamp));
        }
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

        assert_eq!(journal.collateralExpiration, EARLY_CRL_NEXT_UPDATE);
    }

    #[test]
    fn failure_cases_return_expected_error() {
        type FailureCase = (&'static str, TdxVerifierError, fn(&mut TdxVerifierInput));

        let cases: &[FailureCase] = &[
            (
                "bad quote signature",
                TdxVerifierError::QuoteSignatureInvalid(String::new()),
                |input| {
                    let mut quote = input.quote.to_vec();
                    let signature_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
                    quote[signature_offset] ^= 0x01;
                    input.quote = Bytes::from(quote);
                },
            ),
            ("non-TDX quote header", TdxVerifierError::InvalidQuote(String::new()), |input| {
                let mut quote = input.quote.to_vec();
                quote[4..8].copy_from_slice(&0u32.to_le_bytes());
                input.quote = Bytes::from(quote);
            }),
            (
                "unsupported attestation key type",
                TdxVerifierError::InvalidQuote(String::new()),
                |input| {
                    let mut quote = input.quote.to_vec();
                    quote[2..4].copy_from_slice(&1u16.to_le_bytes());
                    input.quote = Bytes::from(quote);
                },
            ),
            (
                "bad QE report signature",
                TdxVerifierError::PckCertChainInvalid(String::new()),
                |input| {
                    let mut quote = input.quote.to_vec();
                    let signature_data_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
                    let qe_report_signature_offset = signature_data_offset
                        + ECDSA_P256_SIGNATURE_LEN
                        + ECDSA_P256_PUBLIC_KEY_BODY_LEN
                        + CERTIFICATION_DATA_HEADER_LEN
                        + QE_REPORT_LEN;
                    quote[qe_report_signature_offset] ^= 0x01;
                    input.quote = Bytes::from(quote);
                },
            ),
            ("wrong root CA hash", TdxVerifierError::RootCaNotTrusted, |input| {
                input.trusted_root_ca_hash = B256::repeat_byte(0xEF);
            }),
            ("expired collateral", TdxVerifierError::CollateralExpired, |input| {
                input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw_with_dates(
                    "UpToDate",
                    EXPIRED_COLLATERAL_NEXT_UPDATE_DATE,
                ));
                resign_tcb_info(input);
            }),
            (
                "revoked collateral signer",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    input.revocation = revocation_evidence("intermediate_crl_revoked_04");
                },
            ),
            ("timestamp outside policy", TdxVerifierError::InvalidTimestamp, |input| {
                input.verification_time = VERIFICATION_TIME + MAX_QUOTE_AGE_SECONDS + 1;
            }),
            ("unsupported TCB status", TdxVerifierError::TcbStatusNotAllowed, |input| {
                input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw("Revoked"));
                resign_tcb_info(input);
            }),
            ("SGX TCB info", TdxVerifierError::TcbInfoInvalid(String::new()), |input| {
                input.collateral.tcb_info.raw = Bytes::from(sgx_tcb_info_raw("UpToDate"));
                resign_tcb_info(input);
            }),
            (
                "malformed TCB info signature",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    input.collateral.tcb_info.signature = Bytes::from(vec![0]);
                },
            ),
            (
                "malformed QE identity signature",
                TdxVerifierError::QeIdentityInvalid(String::new()),
                |input| {
                    input.collateral.qe_identity.signature = Bytes::from(vec![0]);
                },
            ),
            ("stale QE identity", TdxVerifierError::QeIdentityInvalid(String::new()), |input| {
                input.collateral.qe_identity.raw =
                    Bytes::from(qe_identity_raw_with_status("Revoked"));
                resign_qe_identity(input);
            }),
            ("SGX QE identity", TdxVerifierError::QeIdentityInvalid(String::new()), |input| {
                input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_identity(
                    "QE",
                    TDX_QE_IDENTITY_VERSION,
                    "UpToDate",
                ));
                resign_qe_identity(input);
            }),
            ("QVE identity", TdxVerifierError::QeIdentityInvalid(String::new()), |input| {
                input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_identity(
                    "QVE",
                    TDX_QE_IDENTITY_VERSION,
                    "UpToDate",
                ));
                resign_qe_identity(input);
            }),
            ("v1 QE identity", TdxVerifierError::QeIdentityInvalid(String::new()), |input| {
                input.collateral.qe_identity.raw =
                    Bytes::from(qe_identity_raw_with_identity(TDX_QE_IDENTITY_ID, 1, "UpToDate"));
                resign_qe_identity(input);
            }),
            ("malformed public key", TdxVerifierError::MalformedPublicKey, |input| {
                input.expected_public_key = Bytes::from(vec![0x04; 64]);
            }),
            ("report data mismatch", TdxVerifierError::ReportDataMismatch, |input| {
                input.expected_public_key = secp256k1_public_key(2);
            }),
            ("revoked PCK leaf", TdxVerifierError::PckCertChainInvalid(String::new()), |input| {
                input.revocation = revocation_evidence("intermediate_crl_revoked_03");
            }),
            ("downgraded PCK certificate TCB", TdxVerifierError::TcbStatusNotAllowed, |input| {
                input.pck_certificate_chain[2] = fixture_cert("pck_leaf_downgraded_tcb");
                input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw_with_levels(
                    &[tcb_level("UpToDate", 3, 3, 9), tcb_level("OutOfDate", 2, 3, 8)],
                    COLLATERAL_NEXT_UPDATE_DATE,
                ));
                resign_tcb_info(input);
            }),
            ("out-of-date TDX module identity", TdxVerifierError::TcbStatusNotAllowed, |input| {
                input.collateral.tcb_info.raw =
                    Bytes::from(tcb_info_raw_with_levels_and_module_status(
                        &[tcb_level("UpToDate", 3, 3, 9)],
                        COLLATERAL_NEXT_UPDATE_DATE,
                        "OutOfDate",
                        3,
                    ));
                resign_tcb_info(input);
            }),
            (
                "TDX module identity mismatch",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    let raw = String::from_utf8(tcb_info_raw("UpToDate"))
                        .unwrap()
                        .replace("TDX_03", "TDX_04");
                    input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
                    resign_tcb_info(input);
                },
            ),
            (
                "TDX module signer mismatch",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    let mut document: serde_json::Value =
                        serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
                    document["tcbInfo"]["tdxModuleIdentities"][0]["mrsigner"] =
                        serde_json::Value::String("11".repeat(TDX_MEASUREMENT_LEN));
                    input.collateral.tcb_info.raw =
                        Bytes::from(serde_json::to_vec(&document).unwrap());
                    resign_tcb_info(input);
                },
            ),
            (
                "padded serial revocation",
                TdxVerifierError::PckCertChainInvalid(String::new()),
                |input| {
                    input.pck_certificate_chain[2] = fixture_cert("pck_leaf_serial_80");
                    input.revocation = revocation_evidence("intermediate_crl_revoked_80");
                },
            ),
            (
                "wrong collateral signer subject",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    input.collateral.tcb_info.signing_chain[2] =
                        fixture_cert("collateral_leaf_wrong_subject");
                },
            ),
            (
                "wrong collateral key usage",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    input.collateral.tcb_info.signing_chain[2] =
                        fixture_cert("collateral_leaf_key_usage_20");
                },
            ),
            (
                "missing CRL evidence",
                TdxVerifierError::PckCertChainInvalid(String::new()),
                |input| {
                    input.revocation = TdxRevocationEvidence::default();
                },
            ),
            (
                "TCB info platform mismatch",
                TdxVerifierError::TcbInfoInvalid(String::new()),
                |input| {
                    let raw = String::from_utf8(tcb_info_raw("UpToDate"))
                        .unwrap()
                        .replace(FMSPC_HEX, "060504030201");
                    input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
                    resign_tcb_info(input);
                },
            ),
            (
                "bad QE identity signature",
                TdxVerifierError::QeIdentityInvalid(String::new()),
                |input| {
                    let mut signature = input.collateral.qe_identity.signature.to_vec();
                    signature[0] ^= 0x01;
                    input.collateral.qe_identity.signature = Bytes::from(signature);
                },
            ),
        ];

        for (name, expected_error, mutate) in cases {
            let mut input = fixture();
            mutate(&mut input);

            let error = TdxVerifier::verify(&input).expect_err(name);
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(expected_error),
                "{error:?}",
            );
        }
    }
}
