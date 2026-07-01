//! TDX quote parsing and signature verification.

use alloy_primitives::{B256, Bytes};
use sha2::{Digest, Sha256};

use crate::{Result, TdxVerifierError, collateral::CollateralVerifier};

/// Length of a TDX quote header.
pub const TDX_QUOTE_HEADER_LEN: usize = 48;

/// Supported TDX quote version.
pub const TDX_QUOTE_VERSION: u16 = 4;

/// TEE type value for Intel TDX quotes.
pub const TDX_TEE_TYPE: u32 = 0x81;

/// Attestation key type value for ECDSA P-256 quotes.
pub const ECDSA_P256_ATTESTATION_KEY_TYPE: u16 = 2;

/// Length of the TDX report body embedded in a quote.
pub const TDX_REPORT_BODY_LEN: usize = 584;

/// Offset of MRTD inside the TDX report body.
pub const MRTD_OFFSET: usize = 136;

pub(crate) const MRSIGNERSEAM_OFFSET: usize = 64;

pub(crate) const SEAM_ATTRIBUTES_OFFSET: usize = 112;

/// Offset of RTMR measurements inside the TDX report body.
pub const RTMR_OFFSET: usize = 328;

/// Offset of TDREPORT.REPORTDATA inside the TDX report body.
pub const REPORT_DATA_OFFSET: usize = 520;

/// Length of TDX MRTD and RTMR measurements.
pub const TDX_MEASUREMENT_LEN: usize = 48;

pub(crate) const TDX_SEAM_ATTRIBUTES_LEN: usize = 8;

/// Length of `TDREPORT.REPORTDATA`.
pub const TDX_REPORT_DATA_LEN: usize = 64;

pub(crate) const TDX_TEE_TCB_SVN_LEN: usize = 16;

/// Length of an ECDSA P-256 signature in quote data.
pub const ECDSA_P256_SIGNATURE_LEN: usize = 64;

/// Length of an ECDSA P-256 public key body without the uncompressed prefix.
pub const ECDSA_P256_PUBLIC_KEY_BODY_LEN: usize = 64;

/// Length of a QE report embedded in TDX quote signature data.
pub const QE_REPORT_LEN: usize = 384;

pub(crate) const QE_REPORT_MISCSELECT_OFFSET: usize = 16;

pub(crate) const QE_REPORT_MISCSELECT_LEN: usize = 4;

pub(crate) const QE_REPORT_ATTRIBUTES_OFFSET: usize = 48;

pub(crate) const QE_REPORT_ATTRIBUTES_LEN: usize = 16;

pub(crate) const QE_REPORT_MRSIGNER_OFFSET: usize = 128;

pub(crate) const QE_REPORT_MRSIGNER_LEN: usize = 32;

pub(crate) const QE_REPORT_ISV_PROD_ID_OFFSET: usize = 256;

pub(crate) const QE_REPORT_ISV_SVN_OFFSET: usize = 258;

pub(crate) const QE_REPORT_DATA_OFFSET: usize = 320;

pub(crate) const QE_REPORT_DATA_HASH_LEN: usize = 32;

/// Length of the QE authentication data length field.
pub const QE_AUTHENTICATION_DATA_SIZE_LEN: usize = 2;

/// Length of a quote certification data header.
pub const CERTIFICATION_DATA_HEADER_LEN: usize = 6;

/// Certification data type for ECDSA signature auxiliary data.
pub const ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE: u16 = 6;

/// Length of the quote signature data length prefix.
pub const SIGNATURE_DATA_LEN_PREFIX_LEN: usize = 4;

/// Minimum ECDSA signature auxiliary data length.
pub const MIN_AUX_DATA_LEN: usize = QE_REPORT_LEN
    + ECDSA_P256_SIGNATURE_LEN
    + QE_AUTHENTICATION_DATA_SIZE_LEN
    + CERTIFICATION_DATA_HEADER_LEN;

/// Minimum quote signature data length accepted by the parser.
pub const MIN_SIGNATURE_DATA_LEN: usize = ECDSA_P256_SIGNATURE_LEN
    + ECDSA_P256_PUBLIC_KEY_BODY_LEN
    + CERTIFICATION_DATA_HEADER_LEN
    + MIN_AUX_DATA_LEN;

/// Parsed TDX quote fields required by the contract journal.
#[derive(Debug)]
pub struct ParsedTdxQuote {
    /// Raw bytes signed by the quote attestation key.
    pub signed_message: Bytes,
    /// TEE TCB SVN used to select the matching signed TCB info level.
    pub tee_tcb_svn: [u8; TDX_TEE_TCB_SVN_LEN],
    /// MRSIGNERSEAM measurement for the TDX module signer.
    pub mrsigner_seam: [u8; TDX_MEASUREMENT_LEN],
    /// SEAM attributes for the loaded TDX module.
    pub seam_attributes: [u8; TDX_SEAM_ATTRIBUTES_LEN],
    /// MRTD measurement.
    pub mrtd: [u8; TDX_MEASUREMENT_LEN],
    /// RTMR0 measurement.
    pub rtmr0: [u8; TDX_MEASUREMENT_LEN],
    /// RTMR1 measurement.
    pub rtmr1: [u8; TDX_MEASUREMENT_LEN],
    /// RTMR2 measurement.
    pub rtmr2: [u8; TDX_MEASUREMENT_LEN],
    /// RTMR3 measurement.
    pub rtmr3: [u8; TDX_MEASUREMENT_LEN],
    /// Full TDREPORT.REPORTDATA value.
    pub report_data: [u8; TDX_REPORT_DATA_LEN],
    /// P-256 quote signature over `header || report_body`.
    pub quote_signature: Bytes,
    /// Uncompressed P-256 attestation key recovered from quote auth data.
    pub attestation_public_key: Bytes,
    /// QE report signed by the PCK certificate key.
    pub qe_report: Bytes,
    /// P-256 signature over `qe_report` by the PCK certificate key.
    pub qe_report_signature: Bytes,
    /// QE authentication data bound into the QE report data hash.
    pub qe_authentication_data: Bytes,
    /// Certification data type embedded after QE authentication data.
    pub certification_data_type: u16,
    /// Quote certification data embedded after QE authentication data.
    pub certification_data: Bytes,
}

impl ParsedTdxQuote {
    /// Returns the first 32 bytes of `TDREPORT.REPORTDATA`.
    pub fn report_data_prefix(&self) -> B256 {
        B256::from_slice(&self.report_data[..32])
    }

    /// Returns the last 32 bytes of `TDREPORT.REPORTDATA`.
    pub fn report_data_suffix(&self) -> B256 {
        B256::from_slice(&self.report_data[32..])
    }
}

/// Stateless TDX quote parser and signature verifier.
#[derive(Debug)]
pub struct TdxQuote;

impl TdxQuote {
    /// Parses the TDX quote bytes needed by the offchain verifier.
    pub fn parse(raw_quote: &[u8]) -> Result<ParsedTdxQuote> {
        let minimum_len =
            TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + SIGNATURE_DATA_LEN_PREFIX_LEN;
        if raw_quote.len() < minimum_len {
            return Err(TdxVerifierError::InvalidQuote(format!(
                "quote length {} is shorter than minimum {minimum_len}",
                raw_quote.len()
            )));
        }

        let version = u16::from_le_bytes(Self::read_array(raw_quote, 0)?);
        if version != TDX_QUOTE_VERSION {
            return Err(TdxVerifierError::InvalidQuote(format!(
                "unsupported quote version {version}"
            )));
        }
        let attestation_key_type = u16::from_le_bytes(Self::read_array(raw_quote, 2)?);
        if attestation_key_type != ECDSA_P256_ATTESTATION_KEY_TYPE {
            return Err(TdxVerifierError::InvalidQuote(format!(
                "unsupported attestation key type {attestation_key_type}"
            )));
        }
        let tee_type = u32::from_le_bytes(Self::read_array(raw_quote, 4)?);
        if tee_type != TDX_TEE_TYPE {
            return Err(TdxVerifierError::InvalidQuote(format!("unsupported TEE type {tee_type}")));
        }

        let report_start = TDX_QUOTE_HEADER_LEN;
        let report_end = report_start + TDX_REPORT_BODY_LEN;
        let report_body = &raw_quote[report_start..report_end];

        let sig_len_offset = report_end;
        let sig_len = u32::from_le_bytes(Self::read_array(raw_quote, sig_len_offset)?) as usize;
        let sig_data_start = sig_len_offset + SIGNATURE_DATA_LEN_PREFIX_LEN;
        let sig_data_end = sig_data_start.checked_add(sig_len).ok_or_else(|| {
            TdxVerifierError::InvalidQuote("signature data length overflows".into())
        })?;
        if sig_data_end > raw_quote.len() {
            return Err(TdxVerifierError::InvalidQuote(
                "signature data extends past quote length".into(),
            ));
        }

        let sig_data = &raw_quote[sig_data_start..sig_data_end];
        if sig_data.len() < MIN_SIGNATURE_DATA_LEN {
            return Err(TdxVerifierError::InvalidQuote(format!(
                "signature data length {} is shorter than minimum {MIN_SIGNATURE_DATA_LEN}",
                sig_data.len()
            )));
        }

        let (quote_signature, sig_data) = sig_data.split_at(ECDSA_P256_SIGNATURE_LEN);
        let (attestation_key, sig_data) = sig_data.split_at(ECDSA_P256_PUBLIC_KEY_BODY_LEN);
        let (aux_header, aux_data) = sig_data.split_at(CERTIFICATION_DATA_HEADER_LEN);
        let aux_data_type = u16::from_le_bytes(Self::read_array(aux_header, 0)?);
        if aux_data_type != ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE {
            return Err(TdxVerifierError::InvalidQuote(format!(
                "unsupported ECDSA signature auxiliary data type {aux_data_type}"
            )));
        }
        let aux_data_len = u32::from_le_bytes(Self::read_array(aux_header, 2)?) as usize;
        if aux_data.len() != aux_data_len {
            return Err(TdxVerifierError::InvalidQuote(
                "ECDSA signature auxiliary data length does not match signature data length".into(),
            ));
        }

        let (qe_report, aux_data) = aux_data.split_at(QE_REPORT_LEN);
        let (qe_report_signature, aux_data) = aux_data.split_at(ECDSA_P256_SIGNATURE_LEN);
        let (qe_authentication_data_len, aux_data) =
            aux_data.split_at(QE_AUTHENTICATION_DATA_SIZE_LEN);
        let qe_authentication_data_len =
            u16::from_le_bytes(Self::read_array(qe_authentication_data_len, 0)?) as usize;
        let (qe_authentication_data, aux_data) =
            aux_data.split_at_checked(qe_authentication_data_len).ok_or_else(|| {
                TdxVerifierError::InvalidQuote(
                    "signature data is missing QE authentication data".into(),
                )
            })?;
        let (certification_data_header, certification_data) =
            aux_data.split_at_checked(CERTIFICATION_DATA_HEADER_LEN).ok_or_else(|| {
                TdxVerifierError::InvalidQuote(
                    "signature data is missing certification data header".into(),
                )
            })?;
        let certification_data_type =
            u16::from_le_bytes(Self::read_array(certification_data_header, 0)?);
        let certification_data_len =
            u32::from_le_bytes(Self::read_array(certification_data_header, 2)?) as usize;
        if certification_data.len() != certification_data_len {
            return Err(TdxVerifierError::InvalidQuote(
                "certification data length does not match ECDSA signature auxiliary data length"
                    .into(),
            ));
        }

        Ok(ParsedTdxQuote {
            signed_message: Bytes::copy_from_slice(&raw_quote[..report_end]),
            tee_tcb_svn: Self::read_array(report_body, 0)?,
            mrsigner_seam: Self::read_array(report_body, MRSIGNERSEAM_OFFSET)?,
            seam_attributes: Self::read_array(report_body, SEAM_ATTRIBUTES_OFFSET)?,
            mrtd: Self::read_array(report_body, MRTD_OFFSET)?,
            rtmr0: Self::read_array(report_body, RTMR_OFFSET)?,
            rtmr1: Self::read_array(report_body, RTMR_OFFSET + TDX_MEASUREMENT_LEN)?,
            rtmr2: Self::read_array(report_body, RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 2))?,
            rtmr3: Self::read_array(report_body, RTMR_OFFSET + (TDX_MEASUREMENT_LEN * 3))?,
            report_data: Self::read_array(report_body, REPORT_DATA_OFFSET)?,
            quote_signature: Bytes::copy_from_slice(quote_signature),
            attestation_public_key: {
                let mut key = Vec::with_capacity(1 + attestation_key.len());
                key.push(0x04);
                key.extend_from_slice(attestation_key);
                Bytes::from(key)
            },
            qe_report: Bytes::copy_from_slice(qe_report),
            qe_report_signature: Bytes::copy_from_slice(qe_report_signature),
            qe_authentication_data: Bytes::copy_from_slice(qe_authentication_data),
            certification_data_type,
            certification_data: Bytes::copy_from_slice(certification_data),
        })
    }

    /// Verifies the quote signature over `header || report_body`.
    pub fn verify_signature(parsed: &ParsedTdxQuote) -> Result<()> {
        CollateralVerifier::verify_p256_signature(
            &parsed.attestation_public_key,
            &parsed.signed_message,
            &parsed.quote_signature,
            TdxVerifierError::QuoteSignatureInvalid,
            TdxVerifierError::QuoteSignatureInvalid("quote signature verification failed".into()),
        )
    }

    /// Verifies that the PCK certificate key signed the QE report and certified the attestation key.
    pub fn verify_qe_report(parsed: &ParsedTdxQuote, pck_public_key: &[u8]) -> Result<()> {
        CollateralVerifier::verify_p256_signature(
            pck_public_key,
            &parsed.qe_report,
            &parsed.qe_report_signature,
            TdxVerifierError::PckCertChainInvalid,
            TdxVerifierError::PckCertChainInvalid("QE report signature verification failed".into()),
        )?;

        let mut hasher = Sha256::new();
        hasher.update(&parsed.attestation_public_key[1..]);
        hasher.update(&parsed.qe_authentication_data);
        let expected_report_data = hasher.finalize();
        let report_data_hash =
            Self::read_array::<QE_REPORT_DATA_HASH_LEN>(&parsed.qe_report, QE_REPORT_DATA_OFFSET)?;
        if report_data_hash.as_slice() != &expected_report_data[..] {
            return Err(TdxVerifierError::PckCertChainInvalid(
                "QE report data does not bind quote attestation key".into(),
            ));
        }

        Ok(())
    }

    /// Reads a fixed-size array from `bytes`.
    pub fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
        bytes
            .get(offset..)
            .and_then(|bytes| bytes.first_chunk::<N>().copied())
            .ok_or_else(|| TdxVerifierError::InvalidQuote("array read out of bounds".into()))
    }
}
