use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{B256, Bytes};

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
pub struct TdxRuntime {
    signer: TdxSigner,
    quote_provider: Box<dyn TdxQuoteProvider>,
}

impl TdxRuntime {
    /// Creates a runtime with a fresh signer and quote provider.
    pub fn new(quote_provider: impl TdxQuoteProvider + 'static) -> Self {
        Self { signer: TdxSigner::generate(), quote_provider: Box::new(quote_provider) }
    }

    /// Returns the signer's public key.
    pub fn signer_public_key(&self) -> Bytes {
        self.signer.public_key()
    }

    /// Signs arbitrary bytes using the TDX signer.
    pub fn sign(&self, data: &[u8]) -> Result<Bytes> {
        self.signer.sign(data)
    }

    /// Collects a fresh quote using the current system time.
    pub fn signer_quote(&self, attestation_nonce: Option<B256>) -> Result<TdxSignerQuote> {
        let quote_timestamp_millis =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        let public_key = self.signer.public_key();
        let report_data =
            TdxReportData::for_public_key(&public_key, attestation_nonce, quote_timestamp_millis)?;
        let quote = self.quote_provider.quote(&report_data)?;

        Ok(TdxSignerQuote { quote, quote_timestamp_millis })
    }
}

impl std::fmt::Debug for TdxRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdxRuntime").field("signer", &self.signer).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;
    use crate::TDX_REPORT_DATA_LEN;

    struct TestQuoteProvider(Bytes);

    impl TdxQuoteProvider for TestQuoteProvider {
        fn quote(&self, _report_data: &[u8; TDX_REPORT_DATA_LEN]) -> Result<Bytes> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn runtime_returns_quote_and_timestamp() {
        let runtime = TdxRuntime::new(TestQuoteProvider(Bytes::from_static(b"fixture-tdx-quote")));
        let signer_quote = runtime.signer_quote(Some(B256::repeat_byte(0x11))).unwrap();

        assert_eq!(signer_quote.quote, Bytes::from_static(b"fixture-tdx-quote"));
        assert!(signer_quote.quote_timestamp_millis > 0);
    }
}
