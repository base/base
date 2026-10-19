use std::sync::Arc;

use alloy_primitives::Address;
use alloy_rpc_client::ClientBuilder;
use alloy_transport_http::{
    Http,
    reqwest::{self, header::HeaderMap},
};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::info;
use url::Url;

use crate::{
    RemoteSignerHandler,
    signer::remote::cert::{CertificateError, ClientCert},
};

/// Configuration for the remote signer client
///
/// This configuration supports various TLS/certificate scenarios:
///
/// 1. **Basic HTTPS**: Only `endpoint` and `address` are required.
/// 2. **Custom CA**: Provide `ca_cert` to verify servers with custom/self-signed certificates.
/// 3. **Mutual TLS (mTLS)**: Provide `client_cert` with certificate and key paths for client authentication.
/// 4. **Full mTLS with custom CA**: Combine `client_cert` and `ca_cert` for client authentication
///    and custom server verification.
///
/// Certificate formats supported:
/// - PEM format for all certificates and keys
/// - Certificates should be provided as file paths.
///
/// When `client_cert` is set, `start` watches the client certificate and key files and reloads
/// the client when they are modified.
#[derive(Debug, Clone)]
pub struct RemoteSigner {
    /// The URL of the remote signer endpoint
    pub endpoint: Url,
    /// The address of the signer.
    pub address: Address,
    /// Optional client certificate for mTLS (PEM format)
    pub client_cert: Option<ClientCert>,
    /// Optional CA certificate for server verification (PEM format)
    pub ca_cert: Option<std::path::PathBuf>,
    /// Headers to pass to the remote signer.
    pub headers: HeaderMap,
}

/// Errors that can occur when starting a remote signer.
#[derive(Debug, Error)]
pub enum RemoteSignerStartError {
    /// Failed to ping signer
    #[error("Failed to ping signer: {0}")]
    Ping(alloy_transport::TransportError),
    /// HTTP client build error
    #[error("HTTP client build error: {0}")]
    HTTPClientBuild(#[from] reqwest::Error),
    /// Invalid certificate error
    #[error("Invalid certificate: {0}")]
    Certificate(#[from] CertificateError),
    /// Certificate watcher error
    #[error("Certificate watcher error: {0}")]
    CertificateWatcher(#[from] notify::Error),
}

impl RemoteSigner {
    /// Consumes this configuration to build the HTTP client, check `health_status`, and return a handler.
    ///
    /// A failed `health_status` request returns [`RemoteSignerStartError::Ping`].
    /// When `client_cert` is set, starts a watcher for its certificate and key files. Modify events
    /// rebuild and replace the HTTP client; reload errors are logged and keep the old client.
    /// The CA file is not watched. CA changes are read on the next reload triggered by a client
    /// certificate or key modification.
    pub async fn start(self) -> Result<RemoteSignerHandler, RemoteSignerStartError> {
        let http_client = self.build_http_client()?;
        let transport = Http::with_client(http_client, self.endpoint.clone());
        let client = ClientBuilder::default().transport(transport, true);

        let version: String =
            client.request("health_status", ()).await.map_err(RemoteSignerStartError::Ping)?;

        info!(target: "signer", version, "Connected to op-signer server");

        let client = Arc::new(RwLock::new(client));

        let watcher_handle = self.start_certificate_watcher(Arc::clone(&client)).await?;

        Ok(RemoteSignerHandler { client, _watcher_handle: watcher_handle, address: self.address })
    }

    /// Builds an HTTP client with certificate handling for the remote signer
    pub(super) fn build_http_client(&self) -> Result<reqwest::Client, RemoteSignerStartError> {
        let mut client_builder = reqwest::Client::builder();

        if self.client_cert.is_some() || self.ca_cert.is_some() {
            let tls_config = self.build_tls_config()?;
            client_builder = client_builder.use_preconfigured_tls(tls_config);
        }

        client_builder = client_builder.default_headers(self.headers.clone());

        Ok(client_builder.build()?)
    }
}
