//! TDX quote measurement extraction and deterministic local quote fixtures.

use alloy_primitives::{B256, Bytes};
use base_proof_tee_tdx_runtime::{
    Result as TdxRuntimeResult, TdxCollectedQuote, TdxLocalQuoteMetadata, TdxQuoteProvider,
    TdxReportData,
};
use base_proof_tee_tdx_verifier::{
    ParsedTdxQuote, TDX_MEASUREMENT_LEN, TDX_REPORT_DATA_LEN, TdxQuote, TdxVerifier,
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
        TdxQuote::build_mock_quote(
            &self.mrtd,
            &[self.rtmr0, self.rtmr1, self.rtmr2, self.rtmr3],
            report_data,
        )
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
