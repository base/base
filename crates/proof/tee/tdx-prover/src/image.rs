//! TDX quote measurement extraction and deterministic local quote fixtures.

use alloy_primitives::{B256, Bytes};
use base_proof_tee_tdx_runtime::{
    Result as TdxRuntimeResult, TdxCollectedQuote, TdxLocalQuoteMetadata, TdxQuoteProvider,
    TdxReportData,
};
use base_proof_tee_tdx_verifier::{
    CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_ATTESTATION_KEY_TYPE, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
    ECDSA_P256_SIGNATURE_LEN, ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE, MIN_AUX_DATA_LEN,
    MIN_SIGNATURE_DATA_LEN, MRTD_OFFSET, ParsedTdxQuote, QE_AUTHENTICATION_DATA_SIZE_LEN,
    QE_REPORT_LEN, REPORT_DATA_OFFSET, RTMR_OFFSET, SIGNATURE_DATA_LEN_PREFIX_LEN,
    TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN, TDX_QUOTE_VERSION, TDX_REPORT_BODY_LEN,
    TDX_REPORT_DATA_LEN, TDX_TEE_TYPE, TdxQuote, TdxVerifier,
};

use crate::Result;

/// TDX measurements that feed the contract-compatible image hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TdxMeasurements {
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
}

impl TdxMeasurements {
    /// Builds deterministic non-zero measurements for local mock mode.
    pub const fn local_mock() -> Self {
        Self {
            mrtd: [0x11; TDX_MEASUREMENT_LEN],
            rtmr0: [0x22; TDX_MEASUREMENT_LEN],
            rtmr1: [0x33; TDX_MEASUREMENT_LEN],
            rtmr2: [0x44; TDX_MEASUREMENT_LEN],
            rtmr3: [0x55; TDX_MEASUREMENT_LEN],
        }
    }

    /// Extracts TDX image-hash measurements from a parsed quote.
    pub const fn from_parsed_quote(quote: &ParsedTdxQuote) -> Self {
        Self {
            mrtd: quote.mrtd,
            rtmr0: quote.rtmr0,
            rtmr1: quote.rtmr1,
            rtmr2: quote.rtmr2,
            rtmr3: quote.rtmr3,
        }
    }

    /// Parses a quote and extracts TDX image-hash measurements.
    pub fn from_quote(raw_quote: &[u8]) -> Result<Self> {
        let quote = TdxQuote::parse(raw_quote)?;
        Ok(Self::from_parsed_quote(&quote))
    }

    /// Computes the contract-compatible TDX image hash.
    pub fn image_hash(&self) -> B256 {
        TdxVerifier::image_hash(&self.mrtd, &self.rtmr0, &self.rtmr1, &self.rtmr2, &self.rtmr3)
    }

    /// Builds a parseable TDX quote carrying these measurements and the supplied report data.
    pub fn build_mock_quote(&self, report_data: &[u8; TDX_REPORT_DATA_LEN]) -> Bytes {
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
        report[MRTD_OFFSET..MRTD_OFFSET + TDX_MEASUREMENT_LEN].copy_from_slice(&self.mrtd);
        for (i, rtmr) in [&self.rtmr0, &self.rtmr1, &self.rtmr2, &self.rtmr3].iter().enumerate() {
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

        Bytes::from(quote)
    }
}

/// TDX quote provider that builds deterministic parseable quotes for local mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeasuredMockTdxQuoteProvider {
    measurements: TdxMeasurements,
    metadata: TdxLocalQuoteMetadata,
}

impl MeasuredMockTdxQuoteProvider {
    /// Creates a deterministic provider using the supplied measurements.
    pub fn new(measurements: TdxMeasurements) -> Self {
        Self {
            measurements,
            metadata: TdxLocalQuoteMetadata { provider: "mock-tdx".to_owned(), aux_blob: None },
        }
    }

    /// Creates a deterministic provider using local mock measurements.
    pub fn local_mock() -> Self {
        Self::new(TdxMeasurements::local_mock())
    }

    /// Returns the measurements used by generated quotes.
    pub const fn measurements(&self) -> &TdxMeasurements {
        &self.measurements
    }
}

impl TdxQuoteProvider for MeasuredMockTdxQuoteProvider {
    fn quote(&self, report_data: &[u8]) -> TdxRuntimeResult<TdxCollectedQuote> {
        TdxReportData::validate(report_data)?;
        let mut report_data_array = [0u8; TDX_REPORT_DATA_LEN];
        report_data_array.copy_from_slice(report_data);
        let quote = self.measurements.build_mock_quote(&report_data_array);

        Ok(TdxCollectedQuote { quote, metadata: self.metadata.clone() })
    }
}

#[cfg(test)]
mod tests {
    use base_proof_tee_tdx_runtime::TdxQuoteProvider;
    use base_proof_tee_tdx_verifier::TdxVerifier;

    use super::*;

    #[test]
    fn build_mock_quote_emits_parseable_quote_with_measurements() {
        let measurements = TdxMeasurements::local_mock();
        let report_data = [0xAB; TDX_REPORT_DATA_LEN];
        let quote = measurements.build_mock_quote(&report_data);
        let parsed = TdxQuote::parse(&quote).unwrap();

        assert_eq!(parsed.report_data, report_data);
        assert_eq!(TdxMeasurements::from_parsed_quote(&parsed), measurements);
    }

    #[test]
    fn tdx_image_hash_matches_verifier_journal_derivation_for_same_quote() {
        let provider = MeasuredMockTdxQuoteProvider::local_mock();
        let quote = provider.quote(&[0xCD; TDX_REPORT_DATA_LEN]).unwrap().quote;
        let parsed = TdxQuote::parse(&quote).unwrap();
        let measurements = TdxMeasurements::from_quote(&quote).unwrap();

        let verifier_image_hash = TdxVerifier::image_hash(
            &parsed.mrtd,
            &parsed.rtmr0,
            &parsed.rtmr1,
            &parsed.rtmr2,
            &parsed.rtmr3,
        );

        assert_eq!(measurements.image_hash(), verifier_image_hash);
    }
}
