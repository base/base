//! End-to-end TDX quote, collateral, policy, and journal verification.

use alloy_primitives::{Address, keccak256};
use alloy_sol_types::SolValue;
use k256::PublicKey;

use crate::{
    Result, TDXTcbStatus, TDXVerificationResult, TDXVerifierJournal, TdxCertificate,
    TdxPlatformIdentity, TdxQuote, TdxSignedCollateralBody, TdxVerifierError, TdxVerifierInput,
    collateral::CollateralVerifier,
};

/// Stateless TDX attestation verifier.
#[derive(Debug)]
pub struct TdxVerifier;

impl TdxVerifier {
    /// Verifies a TDX quote and collateral bundle into an onchain journal.
    pub fn verify(input: &TdxVerifierInput) -> Result<TDXVerifierJournal> {
        let quote = TdxQuote::parse(&input.quote)?;

        let (pck_leaf_key, pck_crl_expiration) = CollateralVerifier::verify_certificate_chain(
            &input.pck_certificate_chain,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )
        .map_err(|e| {
            if matches!(e, TdxVerifierError::RootCaNotTrusted) {
                e
            } else {
                TdxVerifierError::PckCertChainInvalid(e.to_string())
            }
        })?;

        TdxQuote::verify_qe_report(&quote, &pck_leaf_key)?;
        TdxQuote::verify_signature(&quote)?;

        let tcb_crl_expiration = CollateralVerifier::verify_signed_collateral(
            &input.collateral.tcb_info,
            TdxSignedCollateralBody::TcbInfo,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )?;
        let qe_crl_expiration = CollateralVerifier::verify_signed_collateral(
            &input.collateral.qe_identity,
            TdxSignedCollateralBody::QeIdentity,
            input.trusted_root_ca_hash,
            input.verification_time,
            &input.revocation,
        )?;

        let pck_leaf = input.pck_certificate_chain.last().ok_or_else(|| {
            TdxVerifierError::PckCertChainInvalid("certificate chain is empty".into())
        })?;
        let (pck_platform, pck_tcb) =
            TdxPlatformIdentity::platform_and_tcb_from_pck_certificate_der(&pck_leaf.raw)?;
        let tcb_info_document = input.collateral.tcb_info.tcb_info_document()?;
        tcb_info_document.tcb_info.verify_platform(&pck_platform)?;
        let qe_identity_document = input.collateral.qe_identity.qe_identity_document()?;
        qe_identity_document.enclave_identity.verify_qe_report(&quote)?;

        let tcb_status =
            tcb_info_document.tcb_info.tcb_status_for_quote(&quote, &pck_tcb)?.to_contract_status();
        if tcb_status as u8 == TDXTcbStatus::Unknown as u8
            || !input.allowed_tcb_statuses.iter().any(|allowed| *allowed as u8 == tcb_status as u8)
        {
            return Err(TdxVerifierError::TcbStatusNotAllowed);
        }

        Self::verify_quote_timestamp(
            input.quote_timestamp_millis,
            input.verification_time,
            input.policy.max_quote_age_seconds,
        )?;

        let public_key_hash = Self::validate_public_key(&input.expected_public_key)?;
        let signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        if signer != input.expected_signer {
            return Err(TdxVerifierError::SignerMismatch);
        }
        Self::verify_report_data(&quote, public_key_hash, input.quote_timestamp_millis)?;
        let crl_expiration = pck_crl_expiration.min(tcb_crl_expiration).min(qe_crl_expiration);

        Ok(TDXVerifierJournal {
            result: TDXVerificationResult::Success,
            tcbStatus: tcb_status,
            timestamp: input.quote_timestamp_millis,
            collateralExpiration: Self::collateral_expiration(input, crl_expiration)?,
            rootCaHash: input.trusted_root_ca_hash,
            pckCertHash: pck_leaf.hash(),
            tcbInfoHash: input.collateral.tcb_info.hash(),
            qeIdentityHash: input.collateral.qe_identity.hash(),
            publicKey: input.expected_public_key.clone(),
            signer,
            imageHash: Self::image_hash(
                &quote.mrtd,
                &quote.rtmr0,
                &quote.rtmr1,
                &quote.rtmr2,
                &quote.rtmr3,
            ),
            mrTdHash: keccak256(quote.mrtd),
            reportDataPrefix: quote.report_data_prefix(),
            reportDataSuffix: quote.report_data_suffix(),
        })
    }

    /// ABI-encodes a verified journal for `TDXVerifier.verify` public output.
    pub fn encode_journal(journal: &TDXVerifierJournal) -> Vec<u8> {
        SolValue::abi_encode(journal)
    }

    /// Computes the contract-compatible TDX image hash.
    pub fn image_hash(
        mrtd: &[u8; 48],
        rtmr0: &[u8; 48],
        rtmr1: &[u8; 48],
        rtmr2: &[u8; 48],
        rtmr3: &[u8; 48],
    ) -> alloy_primitives::B256 {
        let mut buf = Vec::with_capacity(48 * 5);
        buf.extend_from_slice(mrtd);
        buf.extend_from_slice(rtmr0);
        buf.extend_from_slice(rtmr1);
        buf.extend_from_slice(rtmr2);
        buf.extend_from_slice(rtmr3);
        keccak256(buf)
    }

    /// Validates and hashes an uncompressed secp256k1 signer public key.
    pub fn validate_public_key(public_key: &[u8]) -> Result<alloy_primitives::B256> {
        if public_key.len() != 65 || public_key.first() != Some(&0x04) {
            return Err(TdxVerifierError::MalformedPublicKey);
        }
        PublicKey::from_sec1_bytes(public_key).map_err(|_| TdxVerifierError::MalformedPublicKey)?;
        Ok(keccak256(&public_key[1..65]))
    }

    /// Computes the expected signed `TDREPORT.REPORTDATA` suffix for a quote timestamp.
    pub fn timestamp_report_data_suffix(timestamp_millis: u64) -> alloy_primitives::B256 {
        let context = b"base-tdx-tee-prover-v1";
        let mut buf = Vec::with_capacity(context.len() + 8);
        buf.extend_from_slice(context);
        buf.extend_from_slice(&timestamp_millis.to_le_bytes());
        keccak256(buf)
    }

    /// Verifies that `TDREPORT.REPORTDATA` binds both the signer key and quote timestamp.
    pub fn verify_report_data(
        quote: &crate::ParsedTdxQuote,
        public_key_hash: alloy_primitives::B256,
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

    /// Returns the earliest collateral expiration accepted into the journal.
    pub fn collateral_expiration(input: &TdxVerifierInput, crl_expiration: u64) -> Result<u64> {
        let certificate_expiration = input
            .pck_certificate_chain
            .iter()
            .chain(input.collateral.tcb_info.signing_chain.iter())
            .chain(input.collateral.qe_identity.signing_chain.iter())
            .try_fold(u64::MAX, |expiration, cert| {
                TdxCertificate::authenticated_from_der(&cert.raw)
                    .map(|cert| expiration.min(cert.not_after))
            })?;
        let (_, tcb_next_update) =
            input.collateral.tcb_info.signed_validity(TdxSignedCollateralBody::TcbInfo)?;
        let (_, qe_next_update) =
            input.collateral.qe_identity.signed_validity(TdxSignedCollateralBody::QeIdentity)?;
        Ok(tcb_next_update.min(qe_next_update).min(certificate_expiration).min(crl_expiration))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, keccak256};
    use alloy_sol_types::SolValue;
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use rstest::rstest;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        TdxCertificate, TdxCollateral, TdxPckTcb, TdxPlatformIdentity, TdxQuotePolicy,
        TdxRevocationEvidence, TdxSignedCollateral,
        collateral::{
            INTEL_TCB_SIGNING_CERT_COMMON_NAME, TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION,
        },
        quote::{
            CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_ATTESTATION_KEY_TYPE,
            ECDSA_P256_PUBLIC_KEY_BODY_LEN, ECDSA_P256_SIGNATURE_LEN,
            ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE, MRTD_OFFSET, QE_REPORT_DATA_HASH_LEN,
            QE_REPORT_DATA_OFFSET, QE_REPORT_LEN, REPORT_DATA_OFFSET, RTMR_OFFSET,
            TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN, TDX_REPORT_BODY_LEN,
            TDX_SEAM_ATTRIBUTES_LEN, TDX_TEE_TCB_SVN_LEN, TDX_TEE_TYPE,
        },
    };

    const VERIFICATION_TIME: u64 = 1_711_111_111;
    const QUOTE_TIMESTAMP_MILLIS: u64 = 1_711_111_000_000;
    const MAX_QUOTE_AGE_SECONDS: u64 = 300;
    const COLLATERAL_NEXT_UPDATE: u64 = 2_051_222_400;
    const EARLY_CRL_NEXT_UPDATE: u64 = 1_893_456_000;
    const COLLATERAL_ISSUE_DATE: &str = "2024-01-01T00:00:00Z";
    const COLLATERAL_NEXT_UPDATE_DATE: &str = "2035-01-01T00:00:00Z";
    const EARLY_CRL_NEXT_UPDATE_DATE: &str = "300101000000Z";
    const EXPIRED_COLLATERAL_NEXT_UPDATE_DATE: &str = "2024-03-01T00:00:00Z";
    const FMSPC_HEX: &str = "010203040506";
    const PCE_ID_HEX: &str = "0009";
    const VALID_SECP256K1_PUBLIC_KEY: [u8; 65] = [
        0x04, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98, 0x48, 0x3a, 0xda, 0x77, 0x26, 0xa3, 0xc4, 0x65, 0x5d, 0xa4, 0xfb, 0xfc,
        0x0e, 0x11, 0x08, 0xa8, 0xfd, 0x17, 0xb4, 0x48, 0xa6, 0x85, 0x54, 0x19, 0x9c, 0x47, 0xd0,
        0x8f, 0xfb, 0x10, 0xd4, 0xb8,
    ];
    const ALTERNATE_SECP256K1_PUBLIC_KEY: [u8; 65] = [
        0x04, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
        0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b, 0x16,
        0xf8, 0x17, 0x98, 0xb7, 0xc5, 0x25, 0x88, 0xd9, 0x5c, 0x3b, 0x9a, 0xa2, 0x5b, 0x04, 0x03,
        0xf1, 0xee, 0xf7, 0x57, 0x02, 0xe8, 0x4b, 0xb7, 0x59, 0x7a, 0xab, 0xe6, 0x63, 0xb8, 0x2f,
        0x6f, 0x04, 0xef, 0x27, 0x77,
    ];

    struct Fixture {
        input: TdxVerifierInput,
        root_hash: B256,
        pck_leaf_hash: B256,
        tcb_hash: B256,
        qe_hash: B256,
    }

    fn signing_key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).expect("fixture signing key must be valid")
    }

    fn public_key_bytes(key: &SigningKey) -> Bytes {
        Bytes::copy_from_slice(key.verifying_key().to_encoded_point(false).as_bytes())
    }

    fn sign(key: &SigningKey, message: &[u8]) -> Bytes {
        let signature: Signature = key.sign(message);
        Bytes::copy_from_slice(&signature.to_bytes())
    }

    fn assert_error_result(error: &TdxVerifierError, expected: TDXVerificationResult) {
        let actual = match error {
            TdxVerifierError::InvalidQuote(_) => TDXVerificationResult::InvalidQuote,
            TdxVerifierError::QuoteSignatureInvalid(_) => {
                TDXVerificationResult::QuoteSignatureInvalid
            }
            TdxVerifierError::RootCaNotTrusted => TDXVerificationResult::RootCaNotTrusted,
            TdxVerifierError::PckCertChainInvalid(_) => TDXVerificationResult::PckCertChainInvalid,
            TdxVerifierError::TcbInfoInvalid(_) => TDXVerificationResult::TcbInfoInvalid,
            TdxVerifierError::QeIdentityInvalid(_) => TDXVerificationResult::QeIdentityInvalid,
            TdxVerifierError::TcbStatusNotAllowed => TDXVerificationResult::TcbStatusNotAllowed,
            TdxVerifierError::CollateralExpired => TDXVerificationResult::CollateralExpired,
            TdxVerifierError::InvalidTimestamp => TDXVerificationResult::InvalidTimestamp,
            TdxVerifierError::MalformedPublicKey
            | TdxVerifierError::SignerMismatch
            | TdxVerifierError::ReportDataMismatch => TDXVerificationResult::ReportDataMismatch,
        };
        assert_eq!(actual as u8, expected as u8, "{error:?}");
    }

    fn der_len(len: usize) -> Vec<u8> {
        if len < 128 {
            return vec![len as u8];
        }

        let bytes = len.to_be_bytes();
        let first_non_zero = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len() - 1);
        let significant = &bytes[first_non_zero..];
        let mut out = vec![0x80 | significant.len() as u8];
        out.extend_from_slice(significant);
        out
    }

    fn der_tag(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&der_len(content.len()));
        out.extend_from_slice(content);
        out
    }

    fn der_sequence(parts: &[Vec<u8>]) -> Vec<u8> {
        der_tag(0x30, &parts.concat())
    }

    fn der_set(parts: &[Vec<u8>]) -> Vec<u8> {
        der_tag(0x31, &parts.concat())
    }

    fn der_integer(bytes: &[u8]) -> Vec<u8> {
        let mut value = bytes.to_vec();
        while value.len() > 1
            && value.first() == Some(&0)
            && value.get(1).is_some_and(|b| b < &0x80)
        {
            value.remove(0);
        }
        if value.first().is_some_and(|byte| byte & 0x80 != 0) {
            value.insert(0, 0);
        }
        der_tag(0x02, &value)
    }

    fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
        let mut value = vec![0];
        value.extend_from_slice(bytes);
        der_tag(0x03, &value)
    }

    fn der_octet_string(bytes: &[u8]) -> Vec<u8> {
        der_tag(0x04, bytes)
    }

    fn der_bool(value: bool) -> Vec<u8> {
        der_tag(0x01, &[if value { 0xff } else { 0 }])
    }

    fn der_utf8(value: &str) -> Vec<u8> {
        der_tag(0x0c, value.as_bytes())
    }

    fn der_utc_time(value: &str) -> Vec<u8> {
        der_tag(0x17, value.as_bytes())
    }

    fn der_oid(nodes: &[u32]) -> Vec<u8> {
        assert!(nodes.len() >= 2);
        let mut body = vec![(nodes[0] * 40 + nodes[1]) as u8];
        for node in &nodes[2..] {
            let mut stack = vec![(node & 0x7f) as u8];
            let mut value = node >> 7;
            while value > 0 {
                stack.push(((value & 0x7f) as u8) | 0x80);
                value >>= 7;
            }
            body.extend(stack.iter().rev());
        }
        der_tag(0x06, &body)
    }

    fn oid_ecdsa_with_sha256() -> Vec<u8> {
        vec![0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]
    }

    fn oid_ec_public_key() -> Vec<u8> {
        vec![0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]
    }

    fn oid_prime256v1() -> Vec<u8> {
        vec![0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]
    }

    fn oid_common_name() -> Vec<u8> {
        vec![0x06, 0x03, 0x55, 0x04, 0x03]
    }

    fn oid_basic_constraints() -> Vec<u8> {
        vec![0x06, 0x03, 0x55, 0x1d, 0x13]
    }

    fn oid_key_usage() -> Vec<u8> {
        vec![0x06, 0x03, 0x55, 0x1d, 0x0f]
    }

    fn oid_intel_sgx_extension() -> Vec<u8> {
        der_oid(&[1, 2, 840, 113741, 1, 13, 1])
    }

    fn oid_intel_tcb_component(component_index: usize) -> Vec<u8> {
        der_oid(&[1, 2, 840, 113741, 1, 13, 1, 2, component_index as u32])
    }

    fn oid_intel_pce_svn() -> Vec<u8> {
        der_oid(&[1, 2, 840, 113741, 1, 13, 1, 2, 17])
    }

    fn oid_intel_pce_id() -> Vec<u8> {
        der_oid(&[1, 2, 840, 113741, 1, 13, 1, 3])
    }

    fn oid_intel_fmspc() -> Vec<u8> {
        der_oid(&[1, 2, 840, 113741, 1, 13, 1, 4])
    }

    fn x509_name(common_name: &str) -> Vec<u8> {
        der_sequence(&[der_set(&[der_sequence(&[oid_common_name(), der_utf8(common_name)])])])
    }

    fn x509_algorithm_identifier() -> Vec<u8> {
        der_sequence(&[oid_ecdsa_with_sha256()])
    }

    fn x509_subject_public_key_info(key: &SigningKey) -> Vec<u8> {
        der_sequence(&[
            der_sequence(&[oid_ec_public_key(), oid_prime256v1()]),
            der_bit_string(key.verifying_key().to_encoded_point(false).as_bytes()),
        ])
    }

    fn x509_extension(oid: Vec<u8>, critical: bool, value: Vec<u8>) -> Vec<u8> {
        der_sequence(&[oid, der_bool(critical), der_octet_string(&value)])
    }

    fn intel_sgx_extension_value(platform: &TdxPlatformIdentity, pck_tcb: &TdxPckTcb) -> Vec<u8> {
        let mut values = pck_tcb
            .sgx_tcb_svn
            .iter()
            .enumerate()
            .map(|(index, svn)| {
                der_sequence(&[oid_intel_tcb_component(index + 1), der_integer(&[*svn])])
            })
            .collect::<Vec<_>>();
        values.push(der_sequence(&[
            oid_intel_pce_svn(),
            der_integer(&pck_tcb.pce_svn.to_be_bytes()),
        ]));
        values.push(der_sequence(&[oid_intel_fmspc(), der_octet_string(&platform.fmspc)]));
        values.push(der_sequence(&[oid_intel_pce_id(), der_octet_string(&platform.pce_id)]));
        der_sequence(&values)
    }

    fn x509_extensions(
        is_ca: bool,
        platform: Option<&TdxPlatformIdentity>,
        pck_tcb: Option<&TdxPckTcb>,
        key_usage_byte: u8,
    ) -> Vec<u8> {
        let basic_constraints_value =
            if is_ca { der_sequence(&[der_bool(true)]) } else { der_sequence(&[]) };
        let key_usage_value = der_bit_string(&[key_usage_byte]);
        let mut extensions = vec![
            x509_extension(oid_basic_constraints(), true, basic_constraints_value),
            x509_extension(oid_key_usage(), true, key_usage_value),
        ];
        if let (Some(platform), Some(pck_tcb)) = (platform, pck_tcb) {
            extensions.push(x509_extension(
                oid_intel_sgx_extension(),
                false,
                intel_sgx_extension_value(platform, pck_tcb),
            ));
        }
        der_tag(0xa3, &der_sequence(&extensions))
    }

    fn der_certificate(
        serial: &[u8],
        subject_name: &str,
        subject_key: &SigningKey,
        issuer_name: &str,
        issuer_key: &SigningKey,
        is_ca: bool,
        pck_platform: Option<(&TdxPlatformIdentity, &TdxPckTcb)>,
    ) -> Vec<u8> {
        let key_usage_byte = if is_ca { 0x04 } else { 0x80 };
        let platform = pck_platform.map(|(platform, _)| platform);
        let pck_tcb = pck_platform.map(|(_, pck_tcb)| pck_tcb);
        let tbs = der_sequence(&[
            der_tag(0xa0, &der_integer(&[2])),
            der_integer(serial),
            x509_algorithm_identifier(),
            x509_name(issuer_name),
            der_sequence(&[der_utc_time("240101000000Z"), der_utc_time("350101000000Z")]),
            x509_name(subject_name),
            x509_subject_public_key_info(subject_key),
            x509_extensions(is_ca, platform, pck_tcb, key_usage_byte),
        ]);
        let signature: Signature = issuer_key.sign(&tbs);
        der_sequence(&[
            tbs,
            x509_algorithm_identifier(),
            der_bit_string(signature.to_der().as_bytes()),
        ])
    }

    fn cert(
        serial: &[u8],
        subject_name: &str,
        subject_key: &SigningKey,
        issuer_name: &str,
        issuer_key: &SigningKey,
        is_ca: bool,
    ) -> TdxCertificate {
        let der = der_certificate(
            serial,
            subject_name,
            subject_key,
            issuer_name,
            issuer_key,
            is_ca,
            None,
        );
        TdxCertificate { raw: Bytes::from(der) }
    }

    fn default_pck_tcb() -> TdxPckTcb {
        TdxPckTcb { sgx_tcb_svn: [3; 16], pce_svn: 9 }
    }

    fn pck_cert(
        serial: &[u8],
        subject_name: &str,
        subject_key: &SigningKey,
        issuer_name: &str,
        issuer_key: &SigningKey,
        platform: &TdxPlatformIdentity,
    ) -> TdxCertificate {
        pck_cert_with_tcb(
            serial,
            subject_name,
            subject_key,
            issuer_name,
            issuer_key,
            platform,
            &default_pck_tcb(),
        )
    }

    fn pck_cert_with_tcb(
        serial: &[u8],
        subject_name: &str,
        subject_key: &SigningKey,
        issuer_name: &str,
        issuer_key: &SigningKey,
        platform: &TdxPlatformIdentity,
        pck_tcb: &TdxPckTcb,
    ) -> TdxCertificate {
        let der = der_certificate(
            serial,
            subject_name,
            subject_key,
            issuer_name,
            issuer_key,
            false,
            Some((platform, pck_tcb)),
        );
        TdxCertificate { raw: Bytes::from(der) }
    }

    fn collateral_cert_with_key_usage(key_usage_byte: u8) -> TdxCertificate {
        let subject_key = signing_key(4);
        let issuer_key = signing_key(2);
        let tbs = der_sequence(&[
            der_tag(0xa0, &der_integer(&[2])),
            der_integer(b"\x07"),
            x509_algorithm_identifier(),
            x509_name("Intel TDX intermediate fixture"),
            der_sequence(&[der_utc_time("240101000000Z"), der_utc_time("350101000000Z")]),
            x509_name(INTEL_TCB_SIGNING_CERT_COMMON_NAME),
            x509_subject_public_key_info(&subject_key),
            x509_extensions(false, None, None, key_usage_byte),
        ]);
        let signature: Signature = issuer_key.sign(&tbs);
        let der = der_sequence(&[
            tbs,
            x509_algorithm_identifier(),
            der_bit_string(signature.to_der().as_bytes()),
        ]);
        TdxCertificate { raw: Bytes::from(der) }
    }

    fn der_crl_with_next_update(
        issuer_name: &str,
        issuer_key: &SigningKey,
        revoked_serials: &[Vec<u8>],
        next_update: &str,
    ) -> Vec<u8> {
        let revoked_certificates = if revoked_serials.is_empty() {
            Vec::new()
        } else {
            let entries = revoked_serials
                .iter()
                .map(|serial| der_sequence(&[der_integer(serial), der_utc_time("240201000000Z")]))
                .collect::<Vec<_>>();
            der_sequence(&entries)
        };
        let mut tbs_parts = vec![
            der_integer(&[1]),
            x509_algorithm_identifier(),
            x509_name(issuer_name),
            der_utc_time("240101000000Z"),
            der_utc_time(next_update),
        ];
        if !revoked_certificates.is_empty() {
            tbs_parts.push(revoked_certificates);
        }
        let tbs = der_sequence(&tbs_parts);
        let signature: Signature = issuer_key.sign(&tbs);
        der_sequence(&[
            tbs,
            x509_algorithm_identifier(),
            der_bit_string(signature.to_der().as_bytes()),
        ])
    }

    fn der_crl(issuer_name: &str, issuer_key: &SigningKey, revoked_serials: &[Vec<u8>]) -> Vec<u8> {
        der_crl_with_next_update(issuer_name, issuer_key, revoked_serials, "350101000000Z")
    }

    fn revocation_evidence(
        root_revoked_serials: &[Vec<u8>],
        intermediate_revoked_serials: &[Vec<u8>],
    ) -> TdxRevocationEvidence {
        let root_key = signing_key(1);
        let intermediate_key = signing_key(2);
        TdxRevocationEvidence {
            certificate_crls: vec![
                Bytes::from(der_crl("Intel TDX Root CA fixture", &root_key, root_revoked_serials)),
                Bytes::from(der_crl(
                    "Intel TDX intermediate fixture",
                    &intermediate_key,
                    intermediate_revoked_serials,
                )),
            ],
        }
    }

    fn revocation_evidence_with_crl_next_update(next_update: &str) -> TdxRevocationEvidence {
        let root_key = signing_key(1);
        let intermediate_key = signing_key(2);
        TdxRevocationEvidence {
            certificate_crls: vec![
                Bytes::from(der_crl_with_next_update(
                    "Intel TDX Root CA fixture",
                    &root_key,
                    &[],
                    next_update,
                )),
                Bytes::from(der_crl_with_next_update(
                    "Intel TDX intermediate fixture",
                    &intermediate_key,
                    &[],
                    next_update,
                )),
            ],
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

    fn signer_public_key() -> Bytes {
        Bytes::from(VALID_SECP256K1_PUBLIC_KEY.to_vec())
    }

    fn build_quote(
        attestation_key: &SigningKey,
        pck_key: &SigningKey,
        public_key: &[u8],
        timestamp_millis: u64,
    ) -> Bytes {
        let mut header = vec![0u8; TDX_QUOTE_HEADER_LEN];
        header[0..2].copy_from_slice(&4u16.to_le_bytes());
        header[2..4].copy_from_slice(&ECDSA_P256_ATTESTATION_KEY_TYPE.to_le_bytes());
        header[4..8].copy_from_slice(&TDX_TEE_TYPE.to_le_bytes());
        header[8..10].copy_from_slice(&7u16.to_le_bytes());
        header[10..12].copy_from_slice(&9u16.to_le_bytes());

        let mut report = vec![0u8; TDX_REPORT_BODY_LEN];
        report[..TDX_TEE_TCB_SVN_LEN].copy_from_slice(&[3; TDX_TEE_TCB_SVN_LEN]);
        report[MRTD_OFFSET..MRTD_OFFSET + TDX_MEASUREMENT_LEN].copy_from_slice(&[0xA0; 48]);
        report[RTMR_OFFSET..RTMR_OFFSET + TDX_MEASUREMENT_LEN].copy_from_slice(&[0xB0; 48]);
        report[RTMR_OFFSET + TDX_MEASUREMENT_LEN..RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 2)]
            .copy_from_slice(&[0xB1; 48]);
        report[RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 2)..RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 3)]
            .copy_from_slice(&[0xB2; 48]);
        report[RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 3)..RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 4)]
            .copy_from_slice(&[0xB3; 48]);
        let public_key_hash = TdxVerifier::validate_public_key(public_key).unwrap();
        report[REPORT_DATA_OFFSET..REPORT_DATA_OFFSET + 32]
            .copy_from_slice(public_key_hash.as_slice());
        report[REPORT_DATA_OFFSET + 32..REPORT_DATA_OFFSET + 64].copy_from_slice(
            TdxVerifier::timestamp_report_data_suffix(timestamp_millis).as_slice(),
        );

        let mut signed_message = Vec::new();
        signed_message.extend_from_slice(&header);
        signed_message.extend_from_slice(&report);

        let quote_signature = sign(attestation_key, &signed_message);
        let attestation_public_key = public_key_bytes(attestation_key);
        let qe_authentication_data = b"fixture-qe-authentication-data";
        let mut qe_report = vec![0u8; QE_REPORT_LEN];
        let mut hasher = Sha256::new();
        hasher.update(&attestation_public_key[1..65]);
        hasher.update(qe_authentication_data);
        let qe_report_data_hash = hasher.finalize();
        qe_report[QE_REPORT_DATA_OFFSET..QE_REPORT_DATA_OFFSET + QE_REPORT_DATA_HASH_LEN]
            .copy_from_slice(&qe_report_data_hash);
        let qe_report_signature = sign(pck_key, &qe_report);
        let certification_data = b"fixture-certification-data";
        let mut aux_data = Vec::new();
        aux_data.extend_from_slice(&qe_report);
        aux_data.extend_from_slice(&qe_report_signature);
        aux_data.extend_from_slice(&(qe_authentication_data.len() as u16).to_le_bytes());
        aux_data.extend_from_slice(qe_authentication_data);
        aux_data.extend_from_slice(&5u16.to_le_bytes());
        aux_data.extend_from_slice(&(certification_data.len() as u32).to_le_bytes());
        aux_data.extend_from_slice(certification_data);

        let mut sig_data = Vec::new();
        sig_data.extend_from_slice(&quote_signature);
        sig_data.extend_from_slice(&attestation_public_key[1..65]);
        sig_data.extend_from_slice(&ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE.to_le_bytes());
        sig_data.extend_from_slice(&(aux_data.len() as u32).to_le_bytes());
        sig_data.extend_from_slice(&aux_data);

        assert!(sig_data.len() >= ECDSA_P256_SIGNATURE_LEN);

        let mut quote = signed_message;
        quote.extend_from_slice(&(sig_data.len() as u32).to_le_bytes());
        quote.extend_from_slice(&sig_data);
        Bytes::from(quote)
    }

    fn tcb_components(svn: u16) -> String {
        (0..TDX_TEE_TCB_SVN_LEN)
            .map(|_| format!(r#"{{"svn":{svn}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn tcb_level(status: &str, sgx_svn: u16, tdx_svn: u16, pce_svn: u16) -> String {
        let sgx_components = tcb_components(sgx_svn);
        let tdx_components = tcb_components(tdx_svn);
        format!(
            r#"{{"tcb":{{"pcesvn":{pce_svn},"sgxtcbcomponents":[{sgx_components}],"tdxtcbcomponents":[{tdx_components}]}},"tcbStatus":"{status}"}}"#
        )
    }

    fn repeated_hex(byte: &str, byte_len: usize) -> String {
        byte.repeat(byte_len)
    }

    fn tdx_module_json() -> String {
        let mrsigner = repeated_hex("00", TDX_MEASUREMENT_LEN);
        let attributes = repeated_hex("00", TDX_SEAM_ATTRIBUTES_LEN);
        let attributes_mask = repeated_hex("ff", TDX_SEAM_ATTRIBUTES_LEN);
        format!(
            r#""tdxModule":{{"mrsigner":"{mrsigner}","attributes":"{attributes}","attributesMask":"{attributes_mask}"}}"#
        )
    }

    fn tdx_module_identity_json(status: &str, isvsvn: u16) -> String {
        let mrsigner = repeated_hex("00", TDX_MEASUREMENT_LEN);
        let attributes = repeated_hex("00", TDX_SEAM_ATTRIBUTES_LEN);
        let attributes_mask = repeated_hex("ff", TDX_SEAM_ATTRIBUTES_LEN);
        format!(
            r#"{{"id":"TDX_03","mrsigner":"{mrsigner}","attributes":"{attributes}","attributesMask":"{attributes_mask}","tcbLevels":[{{"tcb":{{"isvsvn":{isvsvn}}},"tcbStatus":"{status}"}}]}}"#
        )
    }

    fn tcb_info_raw_with_levels_and_module_status(
        levels: &[String],
        next_update: &str,
        module_status: &str,
        module_isvsvn: u16,
    ) -> Vec<u8> {
        let levels = levels.join(",");
        let tdx_module = tdx_module_json();
        let tdx_module_identity = tdx_module_identity_json(module_status, module_isvsvn);
        format!(
            r#"{{"tcbInfo":{{"id":"TDX","teeType":"{TDX_TEE_TYPE:08x}","issueDate":"{COLLATERAL_ISSUE_DATE}","nextUpdate":"{next_update}","fmspc":"{FMSPC_HEX}","pceId":"{PCE_ID_HEX}",{tdx_module},"tdxModuleIdentities":[{tdx_module_identity}],"tcbLevels":[{levels}]}}}}"#
        )
        .into_bytes()
    }

    fn tcb_info_raw_with_levels(levels: &[String], next_update: &str) -> Vec<u8> {
        tcb_info_raw_with_levels_and_module_status(levels, next_update, "UpToDate", 3)
    }

    fn tcb_info_raw_with_dates(status: &str, next_update: &str) -> Vec<u8> {
        tcb_info_raw_with_levels(&[tcb_level(status, 3, 3, 9)], next_update)
    }

    fn tcb_info_raw_for_pck_downgrade() -> Vec<u8> {
        tcb_info_raw_with_levels(
            &[tcb_level("UpToDate", 3, 3, 9), tcb_level("OutOfDate", 2, 3, 8)],
            COLLATERAL_NEXT_UPDATE_DATE,
        )
    }

    fn tcb_info_raw(status: &str) -> Vec<u8> {
        tcb_info_raw_with_dates(status, COLLATERAL_NEXT_UPDATE_DATE)
    }

    fn sgx_tcb_info_raw(status: &str) -> Vec<u8> {
        let components = tcb_components(3);
        format!(
            r#"{{"tcbInfo":{{"id":"SGX","teeType":"00000000","issueDate":"{COLLATERAL_ISSUE_DATE}","nextUpdate":"{COLLATERAL_NEXT_UPDATE_DATE}","fmspc":"{FMSPC_HEX}","pceId":"{PCE_ID_HEX}","tcbLevels":[{{"tcb":{{"pcesvn":9,"sgxtcbcomponents":[{components}]}},"tcbStatus":"{status}"}}]}}}}"#
        )
        .into_bytes()
    }

    fn qe_identity_raw_with_identity(id: &str, version: u16, status: &str) -> Vec<u8> {
        format!(
            r#"{{"enclaveIdentity":{{"id":"{id}","version":{version},"issueDate":"{COLLATERAL_ISSUE_DATE}","nextUpdate":"{COLLATERAL_NEXT_UPDATE_DATE}","miscselect":"00000000","miscselectMask":"ffffffff","attributes":"00000000000000000000000000000000","attributesMask":"ffffffffffffffffffffffffffffffff","mrsigner":"0000000000000000000000000000000000000000000000000000000000000000","isvprodid":0,"tcbLevels":[{{"tcb":{{"isvsvn":0}},"tcbStatus":"{status}"}}]}}}}"#
        )
        .into_bytes()
    }

    fn qe_identity_raw_with_status(status: &str) -> Vec<u8> {
        qe_identity_raw_with_identity(TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION, status)
    }

    fn qe_identity_raw() -> Vec<u8> {
        qe_identity_raw_with_status("UpToDate")
    }

    fn fixture() -> Fixture {
        let root_key = signing_key(1);
        let intermediate_key = signing_key(2);
        let attestation_key = signing_key(3);
        let collateral_key = signing_key(4);
        let pck_key = signing_key(5);
        let platform = TdxPlatformIdentity {
            fmspc: Bytes::from(vec![1, 2, 3, 4, 5, 6]),
            pce_id: Bytes::from(vec![0, 9]),
        };

        let root = cert(
            b"\x01",
            "Intel TDX Root CA fixture",
            &root_key,
            "Intel TDX Root CA fixture",
            &root_key,
            true,
        );
        let intermediate = cert(
            b"\x02",
            "Intel TDX intermediate fixture",
            &intermediate_key,
            "Intel TDX Root CA fixture",
            &root_key,
            true,
        );
        let pck_leaf = pck_cert(
            b"\x03",
            "Intel TDX PCK fixture",
            &pck_key,
            "Intel TDX intermediate fixture",
            &intermediate_key,
            &platform,
        );
        let collateral_leaf = cert(
            b"\x04",
            INTEL_TCB_SIGNING_CERT_COMMON_NAME,
            &collateral_key,
            "Intel TDX intermediate fixture",
            &intermediate_key,
            false,
        );

        let root_hash = root.hash();
        let pck_chain = vec![root.clone(), intermediate.clone(), pck_leaf.clone()];
        let collateral_chain = vec![root, intermediate, collateral_leaf];
        let tcb_info = collateral(
            &tcb_info_raw("UpToDate"),
            TdxSignedCollateralBody::TcbInfo,
            &collateral_key,
            collateral_chain.clone(),
        );
        let qe_identity = collateral(
            &qe_identity_raw(),
            TdxSignedCollateralBody::QeIdentity,
            &collateral_key,
            collateral_chain,
        );
        let public_key = signer_public_key();
        let public_key_hash = TdxVerifier::validate_public_key(&public_key).unwrap();
        let signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
        let quote = build_quote(&attestation_key, &pck_key, &public_key, QUOTE_TIMESTAMP_MILLIS);

        Fixture {
            input: TdxVerifierInput {
                quote,
                pck_certificate_chain: pck_chain,
                collateral: TdxCollateral {
                    tcb_info: tcb_info.clone(),
                    qe_identity: qe_identity.clone(),
                },
                revocation: revocation_evidence(&[], &[]),
                trusted_root_ca_hash: root_hash,
                expected_public_key: public_key,
                expected_signer: signer,
                quote_timestamp_millis: QUOTE_TIMESTAMP_MILLIS,
                verification_time: VERIFICATION_TIME,
                policy: TdxQuotePolicy { max_quote_age_seconds: MAX_QUOTE_AGE_SECONDS },
                allowed_tcb_statuses: vec![TDXTcbStatus::UpToDate],
            },
            root_hash,
            pck_leaf_hash: pck_leaf.hash(),
            tcb_hash: tcb_info.hash(),
            qe_hash: qe_identity.hash(),
        }
    }

    #[test]
    fn verifies_known_good_tdx_quote_fixture_and_emits_solidity_journal() {
        let fixture = fixture();
        let journal = TdxVerifier::verify(&fixture.input).unwrap();

        assert_eq!(journal.result as u8, TDXVerificationResult::Success as u8);
        assert_eq!(journal.tcbStatus as u8, TDXTcbStatus::UpToDate as u8);
        assert_eq!(journal.timestamp, QUOTE_TIMESTAMP_MILLIS);
        assert_eq!(journal.rootCaHash, fixture.root_hash);
        assert_eq!(journal.pckCertHash, fixture.pck_leaf_hash);
        assert_eq!(journal.tcbInfoHash, fixture.tcb_hash);
        assert_eq!(journal.qeIdentityHash, fixture.qe_hash);
        assert_eq!(journal.publicKey, fixture.input.expected_public_key);
        assert_eq!(journal.signer, fixture.input.expected_signer);
        assert_eq!(
            journal.reportDataSuffix,
            TdxVerifier::timestamp_report_data_suffix(QUOTE_TIMESTAMP_MILLIS)
        );
        assert_eq!(
            journal.collateralExpiration, COLLATERAL_NEXT_UPDATE,
            "earliest collateral/cert expiration must be journaled",
        );

        let encoded = TdxVerifier::encode_journal(&journal);
        let decoded = <TDXVerifierJournal as SolValue>::abi_decode_validate(&encoded)
            .expect("journal must decode with Solidity ABI type");
        assert_eq!(decoded.imageHash, journal.imageHash);
        assert_eq!(decoded.mrTdHash, journal.mrTdHash);
        assert_eq!(decoded.reportDataPrefix, journal.reportDataPrefix);
        assert_eq!(decoded.reportDataSuffix, journal.reportDataSuffix);
    }

    #[test]
    fn verifies_tdx_tcb_info_without_tee_type() {
        let mut input = fixture().input;
        let raw = String::from_utf8(tcb_info_raw("UpToDate"))
            .unwrap()
            .replace(r#""teeType":"00000081","#, "");
        input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
        resign_tcb_info(&mut input);

        let journal = TdxVerifier::verify(&input).unwrap();

        assert_eq!(journal.result as u8, TDXVerificationResult::Success as u8);
    }

    #[test]
    fn fixture_quote_collateral_and_journal_abi_round_trips() {
        let fixture = fixture();
        let journal = TdxVerifier::verify(&fixture.input).unwrap();
        let output = Bytes::from(TdxVerifier::encode_journal(&journal));

        let decoded_journal = <TDXVerifierJournal as SolValue>::abi_decode_validate(&output)
            .expect("output must be an ABI-encoded TDX journal");
        assert_eq!(decoded_journal.result as u8, TDXVerificationResult::Success as u8);
        assert_eq!(decoded_journal.signer, fixture.input.expected_signer);
        assert_eq!(decoded_journal.imageHash, journal.imageHash);
    }

    #[test]
    fn collateral_signature_covers_signed_json_body() {
        let mut input = fixture().input;
        let document: serde_json::Value =
            serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
        input.collateral.tcb_info.raw =
            Bytes::from(serde_json::to_string_pretty(&document).unwrap().into_bytes());
        resign_tcb_info(&mut input);

        TdxVerifier::verify(&input).expect("body-signed pretty collateral must verify");

        input.collateral.tcb_info.signature = sign(&signing_key(4), &input.collateral.tcb_info.raw);
        let error =
            TdxVerifier::verify(&input).expect_err("top-level collateral signature must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn qe_identity_signature_must_not_be_bound_to_tcb_info_body() {
        let mut input = fixture().input;
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
        assert_error_result(&error, TDXVerificationResult::QeIdentityInvalid);
    }

    #[test]
    fn image_hash_matches_contract_formula() {
        let parsed = TdxQuote::parse(&fixture().input.quote).unwrap();
        let mut expected = Vec::with_capacity(48 * 5);
        expected.extend_from_slice(&parsed.mrtd);
        expected.extend_from_slice(&parsed.rtmr0);
        expected.extend_from_slice(&parsed.rtmr1);
        expected.extend_from_slice(&parsed.rtmr2);
        expected.extend_from_slice(&parsed.rtmr3);
        assert_eq!(
            TdxVerifier::image_hash(
                &parsed.mrtd,
                &parsed.rtmr0,
                &parsed.rtmr1,
                &parsed.rtmr2,
                &parsed.rtmr3
            ),
            keccak256(expected),
        );
        assert_eq!(keccak256(parsed.mrtd), TdxVerifier::verify(&fixture().input).unwrap().mrTdHash);
    }

    #[test]
    fn malformed_signer_public_key_must_be_on_secp256k1_curve() {
        let mut public_key = vec![0x04];
        public_key.extend_from_slice(&[0; 64]);

        let error =
            TdxVerifier::validate_public_key(&public_key).expect_err("off-curve key must fail");
        assert_error_result(&error, TDXVerificationResult::ReportDataMismatch);
        assert!(matches!(error, TdxVerifierError::MalformedPublicKey));
    }

    #[test]
    fn quote_v5_is_rejected_until_body_layout_is_supported() {
        let mut quote = fixture().input.quote.to_vec();
        quote[0..2].copy_from_slice(&5u16.to_le_bytes());

        let error = TdxQuote::parse(&quote).expect_err("quote v5 must not use v4 body offsets");

        assert!(
            matches!(error, TdxVerifierError::InvalidQuote(message) if message == "unsupported quote version 5")
        );
    }

    #[test]
    fn quote_v4_header_exposes_reserved_bytes() {
        let quote = TdxQuote::parse(&fixture().input.quote).unwrap();

        assert_eq!(quote.header.reserved, [7, 0, 9, 0]);
    }

    #[test]
    fn quote_timestamp_must_match_signed_report_data() {
        let mut input = fixture().input;
        input.quote_timestamp_millis = (VERIFICATION_TIME - 1) * 1_000;

        let error = match TdxVerifier::verify(&input) {
            Ok(_) => panic!("fresh input timestamp must not replay an older signed quote"),
            Err(error) => error,
        };

        assert!(matches!(error, TdxVerifierError::ReportDataMismatch));
    }

    #[test]
    fn quote_timestamp_allows_strictly_past_and_inside_max_age() {
        let previous_second_timestamp_millis = (VERIFICATION_TIME - 1) * 1_000;
        TdxVerifier::verify_quote_timestamp(
            previous_second_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect("quote generated during previous second must be accepted");

        let inside_max_age_timestamp_millis =
            (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS + 1) * 1_000;
        TdxVerifier::verify_quote_timestamp(
            inside_max_age_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect("quote inside max age must be accepted");
    }

    #[test]
    fn quote_timestamp_rejects_contract_boundaries_future_second_and_over_age() {
        let current_timestamp_millis = VERIFICATION_TIME * 1_000;
        let current_error = TdxVerifier::verify_quote_timestamp(
            current_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect_err("quote generated at verification second must fail");

        let exact_max_age_timestamp_millis = (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS) * 1_000;
        let exact_max_age_error = TdxVerifier::verify_quote_timestamp(
            exact_max_age_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect_err("quote exactly at max age must fail");

        let future_timestamp_millis = (VERIFICATION_TIME + 1) * 1_000;
        let future_error = TdxVerifier::verify_quote_timestamp(
            future_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect_err("quote generated after verification second must fail");

        let over_age_timestamp_millis = (VERIFICATION_TIME - MAX_QUOTE_AGE_SECONDS - 1) * 1_000;
        let over_age_error = TdxVerifier::verify_quote_timestamp(
            over_age_timestamp_millis,
            VERIFICATION_TIME,
            MAX_QUOTE_AGE_SECONDS,
        )
        .expect_err("quote older than max age must fail");

        assert!(matches!(current_error, TdxVerifierError::InvalidTimestamp));
        assert!(matches!(exact_max_age_error, TdxVerifierError::InvalidTimestamp));
        assert!(matches!(future_error, TdxVerifierError::InvalidTimestamp));
        assert!(matches!(over_age_error, TdxVerifierError::InvalidTimestamp));
    }

    #[test]
    fn collateral_expiration_includes_earliest_crl_next_update() {
        let mut fixture = fixture();
        fixture.input.revocation =
            revocation_evidence_with_crl_next_update(EARLY_CRL_NEXT_UPDATE_DATE);

        let journal = TdxVerifier::verify(&fixture.input).unwrap();

        assert_eq!(journal.collateralExpiration, EARLY_CRL_NEXT_UPDATE);
    }

    #[rstest]
    #[case::bad_quote_signature(TDXVerificationResult::QuoteSignatureInvalid, |input: &mut TdxVerifierInput| {
        let mut quote = input.quote.to_vec();
        let signature_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
        quote[signature_offset] ^= 0x01;
        input.quote = Bytes::from(quote);
    })]
    #[case::non_tdx_quote_header(TDXVerificationResult::InvalidQuote, |input: &mut TdxVerifierInput| {
        let mut quote = input.quote.to_vec();
        quote[4..8].copy_from_slice(&0u32.to_le_bytes());
        input.quote = Bytes::from(quote);
    })]
    #[case::unsupported_attestation_key_type(TDXVerificationResult::InvalidQuote, |input: &mut TdxVerifierInput| {
        let mut quote = input.quote.to_vec();
        quote[2..4].copy_from_slice(&1u16.to_le_bytes());
        input.quote = Bytes::from(quote);
    })]
    #[case::bad_qe_report_signature(TDXVerificationResult::PckCertChainInvalid, |input: &mut TdxVerifierInput| {
        let mut quote = input.quote.to_vec();
        let signature_data_offset = TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + 4;
        let qe_report_signature_offset =
            signature_data_offset
                + ECDSA_P256_SIGNATURE_LEN
                + ECDSA_P256_PUBLIC_KEY_BODY_LEN
                + CERTIFICATION_DATA_HEADER_LEN
                + QE_REPORT_LEN;
        quote[qe_report_signature_offset] ^= 0x01;
        input.quote = Bytes::from(quote);
    })]
    #[case::wrong_root_ca_hash(TDXVerificationResult::RootCaNotTrusted, |input: &mut TdxVerifierInput| {
        input.trusted_root_ca_hash = B256::repeat_byte(0xEF);
    })]
    #[case::expired_collateral(TDXVerificationResult::CollateralExpired, |input: &mut TdxVerifierInput| {
        input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw_with_dates(
            "UpToDate",
            EXPIRED_COLLATERAL_NEXT_UPDATE_DATE,
        ));
        resign_tcb_info(input);
    })]
    #[case::revoked_collateral_signer(TDXVerificationResult::TcbInfoInvalid, |input: &mut TdxVerifierInput| {
        input.revocation = revocation_evidence(&[], &[vec![0x04]]);
    })]
    #[case::timestamp_outside_policy(TDXVerificationResult::InvalidTimestamp, |input: &mut TdxVerifierInput| {
        input.verification_time = VERIFICATION_TIME + MAX_QUOTE_AGE_SECONDS + 1;
    })]
    #[case::unsupported_tcb_status(TDXVerificationResult::TcbStatusNotAllowed, |input: &mut TdxVerifierInput| {
        input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw("Revoked"));
        resign_tcb_info(input);
    })]
    #[case::sgx_tcb_info_for_tdx_quote(TDXVerificationResult::TcbInfoInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.tcb_info.raw = Bytes::from(sgx_tcb_info_raw("UpToDate"));
        resign_tcb_info(input);
    })]
    #[case::malformed_tcb_info_signature(TDXVerificationResult::TcbInfoInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.tcb_info.signature = Bytes::from(vec![0]);
    })]
    #[case::malformed_qe_identity_signature(TDXVerificationResult::QeIdentityInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.qe_identity.signature = Bytes::from(vec![0]);
    })]
    #[case::stale_qe_identity(TDXVerificationResult::QeIdentityInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_status("Revoked"));
        resign_qe_identity(input);
    })]
    #[case::sgx_qe_identity_for_tdx_quote(TDXVerificationResult::QeIdentityInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_identity("QE", TDX_QE_IDENTITY_VERSION, "UpToDate"));
        resign_qe_identity(input);
    })]
    #[case::qve_identity_for_tdx_quote(TDXVerificationResult::QeIdentityInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_identity("QVE", TDX_QE_IDENTITY_VERSION, "UpToDate"));
        resign_qe_identity(input);
    })]
    #[case::v1_qe_identity_for_tdx_quote(TDXVerificationResult::QeIdentityInvalid, |input: &mut TdxVerifierInput| {
        input.collateral.qe_identity.raw = Bytes::from(qe_identity_raw_with_identity(TDX_QE_IDENTITY_ID, 1, "UpToDate"));
        resign_qe_identity(input);
    })]
    #[case::malformed_public_key(TDXVerificationResult::ReportDataMismatch, |input: &mut TdxVerifierInput| {
        input.expected_public_key = Bytes::from(vec![0x04; 64]);
    })]
    #[case::signer_mismatch(TDXVerificationResult::ReportDataMismatch, |input: &mut TdxVerifierInput| {
        input.expected_signer = Address::repeat_byte(0xFF);
    })]
    #[case::report_data_mismatch(TDXVerificationResult::ReportDataMismatch, |input: &mut TdxVerifierInput| {
        input.expected_public_key = Bytes::from(ALTERNATE_SECP256K1_PUBLIC_KEY.to_vec());
        let public_key_hash = TdxVerifier::validate_public_key(&input.expected_public_key).unwrap();
        input.expected_signer = Address::from_slice(&public_key_hash.as_slice()[12..]);
    })]
    fn failure_cases_return_contract_result(
        #[case] expected_result: TDXVerificationResult,
        #[case] mutate: fn(&mut TdxVerifierInput),
    ) {
        let mut input = fixture().input;
        mutate(&mut input);

        let error = TdxVerifier::verify(&input).expect_err("fixture mutation must fail");
        assert_error_result(&error, expected_result);
    }

    #[test]
    fn pck_revocation_fails_chain_validation() {
        let mut input = fixture().input;
        input.revocation = revocation_evidence(&[], &[vec![0x03]]);

        let error = TdxVerifier::verify(&input).expect_err("revoked PCK leaf must fail");
        assert_error_result(&error, TDXVerificationResult::PckCertChainInvalid);
    }

    #[test]
    fn tcb_status_uses_pck_certificate_tcb_components() {
        let mut input = fixture().input;
        input.pck_certificate_chain[2] = pck_cert_with_tcb(
            b"\x03",
            "Intel TDX PCK fixture",
            &signing_key(5),
            "Intel TDX intermediate fixture",
            &signing_key(2),
            &TdxPlatformIdentity {
                fmspc: Bytes::from(vec![1, 2, 3, 4, 5, 6]),
                pce_id: Bytes::from(vec![0, 9]),
            },
            &TdxPckTcb { sgx_tcb_svn: [2; 16], pce_svn: 8 },
        );
        input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw_for_pck_downgrade());
        resign_tcb_info(&mut input);

        let error = TdxVerifier::verify(&input)
            .expect_err("downgraded PCK certificate TCB must not select UpToDate");
        assert_error_result(&error, TDXVerificationResult::TcbStatusNotAllowed);
    }

    #[test]
    fn tcb_status_uses_tdx_module_identity_status() {
        let mut input = fixture().input;
        input.collateral.tcb_info.raw = Bytes::from(tcb_info_raw_with_levels_and_module_status(
            &[tcb_level("UpToDate", 3, 3, 9)],
            COLLATERAL_NEXT_UPDATE_DATE,
            "OutOfDate",
            3,
        ));
        resign_tcb_info(&mut input);

        let error = TdxVerifier::verify(&input)
            .expect_err("out-of-date TDX module identity must affect TCB status");
        assert_error_result(&error, TDXVerificationResult::TcbStatusNotAllowed);
    }

    #[test]
    fn tcb_info_must_match_tdx_module_identity_version() {
        let mut input = fixture().input;
        let raw = String::from_utf8(tcb_info_raw("UpToDate")).unwrap().replace("TDX_03", "TDX_04");
        input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
        resign_tcb_info(&mut input);

        let error =
            TdxVerifier::verify(&input).expect_err("TDX module identity mismatch must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn tcb_info_must_match_tdx_module_signer() {
        let mut input = fixture().input;
        let mut document: serde_json::Value =
            serde_json::from_slice(&input.collateral.tcb_info.raw).unwrap();
        document["tcbInfo"]["tdxModuleIdentities"][0]["mrsigner"] =
            serde_json::Value::String(repeated_hex("11", TDX_MEASUREMENT_LEN));
        input.collateral.tcb_info.raw = Bytes::from(serde_json::to_vec(&document).unwrap());
        resign_tcb_info(&mut input);

        let error = TdxVerifier::verify(&input).expect_err("TDX module signer mismatch must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn revocation_matches_der_serial_with_positive_padding() {
        let mut input = fixture().input;
        input.pck_certificate_chain[2] = pck_cert(
            b"\x80",
            "Intel TDX PCK fixture",
            &signing_key(5),
            "Intel TDX intermediate fixture",
            &signing_key(2),
            &TdxPlatformIdentity {
                fmspc: Bytes::from(vec![1, 2, 3, 4, 5, 6]),
                pce_id: Bytes::from(vec![0, 9]),
            },
        );
        input.revocation = revocation_evidence(&[], &[vec![0x80]]);

        let error = TdxVerifier::verify(&input).expect_err("padded serial revocation must fail");
        assert_error_result(&error, TDXVerificationResult::PckCertChainInvalid);
    }

    #[test]
    fn collateral_signer_must_have_expected_subject() {
        let mut input = fixture().input;
        let wrong_leaf = cert(
            b"\x06",
            "Intel TDX unrelated leaf fixture",
            &signing_key(4),
            "Intel TDX intermediate fixture",
            &signing_key(2),
            false,
        );
        input.collateral.tcb_info.signing_chain[2] = wrong_leaf;

        let error = TdxVerifier::verify(&input).expect_err("wrong collateral signer must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn collateral_signer_must_allow_digital_signatures() {
        let mut input = fixture().input;
        input.collateral.tcb_info.signing_chain[2] = collateral_cert_with_key_usage(0x20);

        let error = TdxVerifier::verify(&input).expect_err("wrong collateral key usage must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn missing_signed_crl_evidence_fails_chain_validation() {
        let mut input = fixture().input;
        input.revocation = TdxRevocationEvidence::default();

        let error = TdxVerifier::verify(&input).expect_err("missing CRL evidence must fail");
        assert_error_result(&error, TDXVerificationResult::PckCertChainInvalid);
    }

    #[test]
    fn tcb_info_must_match_pck_platform_identity() {
        let mut input = fixture().input;
        let raw =
            String::from_utf8(tcb_info_raw("UpToDate")).unwrap().replace(FMSPC_HEX, "060504030201");
        input.collateral.tcb_info.raw = Bytes::from(raw.into_bytes());
        resign_tcb_info(&mut input);

        let error = TdxVerifier::verify(&input).expect_err("TCB info platform mismatch must fail");
        assert_error_result(&error, TDXVerificationResult::TcbInfoInvalid);
    }

    #[test]
    fn qe_identity_signature_failure_is_reported() {
        let mut input = fixture().input;
        let mut signature = input.collateral.qe_identity.signature.to_vec();
        signature[0] ^= 0x01;
        input.collateral.qe_identity.signature = Bytes::from(signature);

        let error = TdxVerifier::verify(&input).expect_err("bad QE identity signature must fail");
        assert_error_result(&error, TDXVerificationResult::QeIdentityInvalid);
    }
}
