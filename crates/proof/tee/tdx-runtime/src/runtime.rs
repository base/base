use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::Bytes;

use crate::{Result, TdxQuoteProvider, TdxReportData, TdxSigner};

/// TDX signer quote response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TdxSignerQuote {
    /// Raw TDX quote bytes.
    pub quote: Bytes,
    /// Quote collection timestamp in milliseconds.
    pub quote_timestamp_millis: u64,
}

/// TDX runtime owning signer identity and quote collection.
pub struct TdxRuntime<P> {
    signer: TdxSigner,
    quote_provider: P,
}

impl<P> TdxRuntime<P> {
    /// Creates a runtime with a fresh signer and quote provider.
    pub fn new(quote_provider: P) -> Self {
        Self { signer: TdxSigner::generate(), quote_provider }
    }

    /// Returns the signer's public key.
    pub fn signer_public_key(&self) -> Bytes {
        self.signer.public_key()
    }

    /// Signs arbitrary bytes using the TDX signer.
    pub fn sign(&self, data: &[u8]) -> Result<Bytes> {
        self.signer.sign(data)
    }

    /// Returns the current Unix timestamp in milliseconds.
    pub fn now_millis() -> Result<u64> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
    }
}

impl<P: TdxQuoteProvider> TdxRuntime<P> {
    /// Collects a fresh quote using the current system time.
    pub fn signer_quote(&self) -> Result<TdxSignerQuote> {
        let quote_timestamp_millis = Self::now_millis()?;
        let public_key = self.signer.public_key();
        let report_data = TdxReportData::for_public_key(&public_key, quote_timestamp_millis)?;
        let quote = self.quote_provider.quote(&report_data)?;

        Ok(TdxSignerQuote { quote, quote_timestamp_millis })
    }
}

impl<P: fmt::Debug> fmt::Debug for TdxRuntime<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdxRuntime")
            .field("signer_address", &self.signer.address())
            .field("quote_provider", &self.quote_provider)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;
    use crate::{ConfigfsTdxQuoteProvider, TDX_REPORT_DATA_LEN};

    #[derive(Debug)]
    struct TestQuoteProvider(Bytes);

    impl TdxQuoteProvider for TestQuoteProvider {
        fn quote(&self, _report_data: &[u8; TDX_REPORT_DATA_LEN]) -> Result<Bytes> {
            Ok(self.0.clone())
        }
    }

    fn test_runtime() -> TdxRuntime<TestQuoteProvider> {
        TdxRuntime::new(TestQuoteProvider(Bytes::from_static(b"fixture-tdx-quote")))
    }

    #[test]
    fn runtime_returns_quote_and_timestamp() {
        let runtime = test_runtime();
        let signer_quote = runtime.signer_quote().unwrap();

        assert_eq!(signer_quote.quote, Bytes::from_static(b"fixture-tdx-quote"));
        assert!(signer_quote.quote_timestamp_millis > 0);
    }

    #[test]
    fn runtime_debug_does_not_expose_private_key_material() {
        let runtime = test_runtime();
        let debug = format!("{runtime:?}");

        assert!(debug.contains("TdxRuntime"));
        assert!(debug.contains("signer_address"));
        assert!(!debug.contains("signer:"));
    }

    #[test]
    #[ignore = "requires a real TDX guest with Linux TSM/configfs mounted"]
    fn real_tdx_guest_smoke_test_collects_quote_for_generated_signer() {
        let provider = ConfigfsTdxQuoteProvider::new("base-tdx-runtime-smoke");
        let runtime = TdxRuntime::new(provider);

        let signer_quote = runtime.signer_quote().unwrap();

        assert!(!signer_quote.quote.is_empty());
    }
}
