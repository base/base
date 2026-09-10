use http::HeaderMap;
use jsonrpsee_http_client::HttpResponse;

/// General purpose trait to validate Http Authorization headers. It's supposed to be integrated as
/// a validator trait into an [`AuthLayer`].
pub trait AuthValidator {
    /// This function is invoked by the [`AuthLayer`] to perform validation on Http headers.
    /// The result conveys validation errors in the form of an Http response.
    #[expect(clippy::result_large_err)]
    fn validate(&self, headers: &HeaderMap) -> Result<(), HttpResponse>;
}
