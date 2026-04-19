//! JWT validation utilities.

#[cfg(feature = "engine-validation")]
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
#[cfg(feature = "engine-validation")]
use backon::{ExponentialBuilder, Retryable};
#[cfg(feature = "engine-validation")]
use base_common_network::Base;
#[cfg(feature = "engine-validation")]
use base_common_provider::BaseEngineApi;
#[cfg(feature = "engine-validation")]
use base_consensus_engine::BaseEngineClient;
#[cfg(feature = "engine-validation")]
use tracing::{debug, error};

#[cfg(feature = "engine-validation")]
use crate::JwtValidationError;

/// A JWT validator that can verify JWT secrets against an engine API.
#[derive(Debug, Clone, Copy)]
pub struct JwtValidator {
    secret: JwtSecret,
}

impl JwtValidator {
    /// Creates a new JWT validator with the given secret.
    pub const fn new(secret: JwtSecret) -> Self {
        Self { secret }
    }

    /// Returns the underlying JWT secret.
    pub const fn secret(&self) -> JwtSecret {
        self.secret
    }

    /// Consumes the validator and returns the JWT secret.
    pub const fn into_inner(self) -> JwtSecret {
        self.secret
    }

    /// Check if an error is related to JWT signature validation.
    ///
    /// Walks the error chain to detect JWT authentication failures by
    /// looking for common error message patterns.
    pub fn is_jwt_signature_error(error: &dyn std::error::Error) -> bool {
        let mut source = Some(error);
        while let Some(err) = source {
            let err_str = err.to_string().to_lowercase();
            if err_str.contains("signature invalid")
                || (err_str.contains("jwt") && err_str.contains("invalid"))
                || err_str.contains("unauthorized")
                || err_str.contains("authentication failed")
            {
                return true;
            }
            source = err.source();
        }
        false
    }

    /// Helper to check JWT signature error from `eyre::Error` (for retry condition).
    #[cfg(feature = "engine-validation")]
    pub fn is_jwt_signature_error_from_eyre(error: &eyre::Error) -> bool {
        Self::is_jwt_signature_error(error.as_ref() as &dyn std::error::Error)
    }
}

#[cfg(feature = "engine-validation")]
impl JwtValidator {
    /// Validates the JWT secret by exchanging capabilities with an engine API.
    ///
    /// Uses exponential backoff for transient failures, but fails immediately
    /// on authentication errors (invalid JWT signature).
    ///
    /// # Arguments
    /// * `engine_url` - The URL of the engine API endpoint. Supports HTTP(S), WS(S), and
    ///   `file://` URLs. WebSocket URLs are normalized to HTTP(S) for validation because the
    ///   engine capability exchange is not served over WS. In the IPC case this checks engine
    ///   reachability and capability exchange over the socket path; JWT authentication is not
    ///   exercised because IPC access is gated by filesystem permissions instead.
    ///
    /// # Returns
    /// * `Ok(JwtSecret)` - The validated JWT secret
    /// * `Err(JwtValidationError::InvalidSignature)` - JWT authentication failed
    /// * `Err(JwtValidationError::CapabilityExchange(_))` - Transient error after retries
    pub async fn validate_with_engine(
        self,
        engine_url: url::Url,
    ) -> Result<JwtSecret, JwtValidationError> {
        let engine_url = Self::normalize_engine_url(engine_url)?;

        let exchange = || async {
            // Recreate the client on every retry because the IPC transport connects eagerly.
            // This allows startup retries to cover the initial socket connect when the engine
            // listener is not ready yet. HTTP URLs are lazy and would not need this, but
            // recreating them is harmless.
            let engine = BaseEngineClient::<RootProvider, RootProvider<Base>>::rpc_client::<Base>(
                engine_url.clone(),
                self.secret,
            )
            .await
            .map_err(|e| eyre::eyre!(JwtValidationError::CapabilityExchange(e.to_string())))?;

            match <RootProvider<Base> as BaseEngineApi>::exchange_capabilities(&engine, vec![])
                .await
            {
                Ok(_) => {
                    debug!("Successfully exchanged capabilities with engine");
                    Ok(self.secret)
                }
                Err(e) => {
                    if Self::is_jwt_signature_error(&e) {
                        error!(
                            "Engine API JWT secret differs from the one specified by --l2.jwt-secret/--l2.jwt-secret-encoded"
                        );
                        error!(
                            "Ensure that the JWT secret file specified is correct (by default it is `jwt.hex` in the current directory)"
                        );
                        return Err(JwtValidationError::InvalidSignature.into());
                    }
                    Err(JwtValidationError::CapabilityExchange(e.to_string()).into())
                }
            }
        };

        exchange
            .retry(ExponentialBuilder::default())
            .when(|e: &eyre::Error| !Self::is_jwt_signature_error_from_eyre(e))
            .notify(|_, duration| {
                debug!(duration = ?duration, "Retrying engine capability handshake");
            })
            .await
            .map_err(|e| {
                // Convert eyre::Error back to JwtValidationError
                if Self::is_jwt_signature_error_from_eyre(&e) {
                    JwtValidationError::InvalidSignature
                } else {
                    JwtValidationError::CapabilityExchange(e.to_string())
                }
            })
    }

    /// Normalizes an engine URL by converting WebSocket schemes to HTTP.
    ///
    /// - `ws://` becomes `http://`
    /// - `wss://` becomes `https://`
    /// - `http://`, `https://`, and `file://` are returned unchanged
    ///
    /// # Errors
    /// Returns an error if the URL scheme is unsupported.
    fn normalize_engine_url(mut url: url::Url) -> Result<url::Url, JwtValidationError> {
        match url.scheme() {
            "http" | "https" | "file" => Ok(url),
            "ws" => {
                debug!("Converting WebSocket URL to HTTP for engine validation");
                url.set_scheme("http").map_err(|()| {
                    JwtValidationError::CapabilityExchange(
                        "Failed to convert ws:// to http://".to_string(),
                    )
                })?;
                Ok(url)
            }
            "wss" => {
                debug!("Converting secure WebSocket URL to HTTPS for engine validation");
                url.set_scheme("https").map_err(|()| {
                    JwtValidationError::CapabilityExchange(
                        "Failed to convert wss:// to https://".to_string(),
                    )
                })?;
                Ok(url)
            }
            scheme => Err(JwtValidationError::CapabilityExchange(format!(
                "Unsupported URL scheme '{scheme}'. Expected http, https, ws, wss, or file"
            ))),
        }
    }
}

#[cfg(all(test, feature = "engine-validation"))]
mod tests {
    use std::env;

    use url::Url;

    use super::*;

    #[test]
    fn normalize_http_url_unchanged() {
        let url = Url::parse("http://localhost:8551").unwrap();
        let result = JwtValidator::normalize_engine_url(url).unwrap();
        assert_eq!(result.scheme(), "http");
        assert_eq!(result.host_str(), Some("localhost"));
        assert_eq!(result.port(), Some(8551));
    }

    #[test]
    fn normalize_https_url_unchanged() {
        let url = Url::parse("https://localhost:8551").unwrap();
        let result = JwtValidator::normalize_engine_url(url).unwrap();
        assert_eq!(result.scheme(), "https");
    }

    #[test]
    fn normalize_file_url_unchanged() {
        let path = env::temp_dir().join("jwt-validator-engine.ipc");
        let url = Url::from_file_path(&path).unwrap();
        let result = JwtValidator::normalize_engine_url(url.clone()).unwrap();
        assert_eq!(result, url);
    }

    #[test]
    fn normalize_ws_to_http() {
        let url = Url::parse("ws://localhost:8551").unwrap();
        let result = JwtValidator::normalize_engine_url(url).unwrap();
        assert_eq!(result.scheme(), "http");
        assert_eq!(result.host_str(), Some("localhost"));
        assert_eq!(result.port(), Some(8551));
    }

    #[test]
    fn normalize_wss_to_https() {
        let url = Url::parse("wss://localhost:8551/path").unwrap();
        let result = JwtValidator::normalize_engine_url(url).unwrap();
        assert_eq!(result.scheme(), "https");
        assert_eq!(result.host_str(), Some("localhost"));
        assert_eq!(result.port(), Some(8551));
        assert_eq!(result.path(), "/path");
    }

    #[test]
    fn normalize_unsupported_scheme_error() {
        let url = Url::parse("ftp://localhost:8551").unwrap();
        let result = JwtValidator::normalize_engine_url(url);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            JwtValidationError::CapabilityExchange(msg) => {
                assert!(msg.contains("Unsupported URL scheme"));
                assert!(msg.contains("ftp"));
            }
            _ => panic!("Expected CapabilityExchange error"),
        }
    }

    #[test]
    fn normalize_preserves_path_and_query() {
        let url = Url::parse("ws://localhost:8551/api?key=value").unwrap();
        let result = JwtValidator::normalize_engine_url(url).unwrap();
        assert_eq!(result.scheme(), "http");
        assert_eq!(result.path(), "/api");
        assert_eq!(result.query(), Some("key=value"));
    }
}
