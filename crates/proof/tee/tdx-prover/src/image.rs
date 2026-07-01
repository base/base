//! Deterministic local TDX quote fixtures.

use alloy_primitives::Bytes;
use base_proof_tee_tdx_runtime::{Result as TdxRuntimeResult, TdxQuoteProvider};
use base_proof_tee_tdx_verifier::{
    CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_ATTESTATION_KEY_TYPE, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
    ECDSA_P256_SIGNATURE_LEN, ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE, MIN_AUX_DATA_LEN,
    MIN_SIGNATURE_DATA_LEN, MRTD_OFFSET, QE_AUTHENTICATION_DATA_SIZE_LEN, QE_REPORT_LEN,
    REPORT_DATA_OFFSET, RTMR_OFFSET, SIGNATURE_DATA_LEN_PREFIX_LEN, TDX_MEASUREMENT_LEN,
    TDX_QUOTE_HEADER_LEN, TDX_QUOTE_VERSION, TDX_REPORT_BODY_LEN, TDX_REPORT_DATA_LEN,
    TDX_TEE_TYPE,
};

const LOCAL_MRTD: [u8; TDX_MEASUREMENT_LEN] = [0x11; TDX_MEASUREMENT_LEN];
const LOCAL_RTMR0: [u8; TDX_MEASUREMENT_LEN] = [0x22; TDX_MEASUREMENT_LEN];
const LOCAL_RTMR1: [u8; TDX_MEASUREMENT_LEN] = [0x33; TDX_MEASUREMENT_LEN];
const LOCAL_RTMR2: [u8; TDX_MEASUREMENT_LEN] = [0x44; TDX_MEASUREMENT_LEN];
const LOCAL_RTMR3: [u8; TDX_MEASUREMENT_LEN] = [0x55; TDX_MEASUREMENT_LEN];

/// TDX quote provider for deterministic local quote fixtures.
#[derive(Debug)]
pub struct TdxMeasurements;

impl TdxQuoteProvider for TdxMeasurements {
    /// Builds a parseable TDX quote carrying these measurements and the supplied report data.
    fn quote(&self, report_data: &[u8; TDX_REPORT_DATA_LEN]) -> TdxRuntimeResult<Bytes> {
        let mut quote = vec![
            0u8;
            TDX_QUOTE_HEADER_LEN
                + TDX_REPORT_BODY_LEN
                + SIGNATURE_DATA_LEN_PREFIX_LEN
                + MIN_SIGNATURE_DATA_LEN
        ];

        quote[0..2].copy_from_slice(&TDX_QUOTE_VERSION.to_le_bytes());
        quote[2..4].copy_from_slice(&ECDSA_P256_ATTESTATION_KEY_TYPE.to_le_bytes());
        quote[4..8].copy_from_slice(&TDX_TEE_TYPE.to_le_bytes());

        let report = &mut quote[TDX_QUOTE_HEADER_LEN..TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN];
        report[MRTD_OFFSET..MRTD_OFFSET + TDX_MEASUREMENT_LEN].copy_from_slice(&LOCAL_MRTD);
        for (i, rtmr) in [&LOCAL_RTMR0, &LOCAL_RTMR1, &LOCAL_RTMR2, &LOCAL_RTMR3].iter().enumerate()
        {
            let offset = RTMR_OFFSET + i * TDX_MEASUREMENT_LEN;
            report[offset..offset + TDX_MEASUREMENT_LEN].copy_from_slice(*rtmr);
        }
        report[REPORT_DATA_OFFSET..REPORT_DATA_OFFSET + TDX_REPORT_DATA_LEN]
            .copy_from_slice(report_data);

        let signature_data_start =
            TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN + SIGNATURE_DATA_LEN_PREFIX_LEN;
        quote[TDX_QUOTE_HEADER_LEN + TDX_REPORT_BODY_LEN..signature_data_start]
            .copy_from_slice(&(MIN_SIGNATURE_DATA_LEN as u32).to_le_bytes());

        let signature_data =
            &mut quote[signature_data_start..signature_data_start + MIN_SIGNATURE_DATA_LEN];
        let aux_header_offset = ECDSA_P256_SIGNATURE_LEN + ECDSA_P256_PUBLIC_KEY_BODY_LEN;
        signature_data[aux_header_offset..aux_header_offset + 2]
            .copy_from_slice(&ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE.to_le_bytes());
        signature_data[aux_header_offset + 2..aux_header_offset + CERTIFICATION_DATA_HEADER_LEN]
            .copy_from_slice(&(MIN_AUX_DATA_LEN as u32).to_le_bytes());
        let certification_header_offset = aux_header_offset
            + CERTIFICATION_DATA_HEADER_LEN
            + QE_REPORT_LEN
            + ECDSA_P256_SIGNATURE_LEN
            + QE_AUTHENTICATION_DATA_SIZE_LEN;
        signature_data[certification_header_offset + 2
            ..certification_header_offset + CERTIFICATION_DATA_HEADER_LEN]
            .copy_from_slice(&0u32.to_le_bytes());

        Ok(Bytes::from(quote))
    }
}

#[cfg(test)]
mod tests {
    use base_proof_tee_tdx_verifier::TdxQuote;

    use super::*;

    #[test]
    fn quote_emits_parseable_quote_with_measurements() {
        let measurements = TdxMeasurements;
        let report_data = [0xAB; TDX_REPORT_DATA_LEN];
        let quote = measurements.quote(&report_data).unwrap();
        let parsed = TdxQuote::parse(&quote).unwrap();

        assert_eq!(parsed.report_data, report_data);
        assert_eq!(parsed.mrtd, LOCAL_MRTD);
        assert_eq!(parsed.rtmr0, LOCAL_RTMR0);
        assert_eq!(parsed.rtmr1, LOCAL_RTMR1);
        assert_eq!(parsed.rtmr2, LOCAL_RTMR2);
        assert_eq!(parsed.rtmr3, LOCAL_RTMR3);
    }
}
