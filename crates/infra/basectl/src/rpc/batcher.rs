//! Batcher admin JSON-RPC client.

use std::time::Duration;

use jsonrpsee::{
    core::client::{ClientT, Error as JsonRpcClientError},
    http_client::HttpClientBuilder,
    rpc_params,
};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use crate::errors::BatcherCommandError;

/// Runtime state returned by `admin_getBatcherStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct BatcherStatus {
    /// Whether batch submission is stopped.
    pub stopped: bool,
    /// Number of L1 transactions submitted but not yet confirmed.
    pub in_flight: u64,
    /// Estimated unsubmitted DA backlog in bytes.
    pub da_backlog_bytes: u64,
}

/// Batcher admin RPC client helpers.
#[derive(Debug)]
pub struct BatcherClient;

impl BatcherClient {
    /// Timeout of the calls the batcher answers right away.
    pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

    /// Timeout of `admin_stopBatcher`, which answers only once no submission is in flight.
    ///
    /// The batcher gives up after its own drain timeout, 96s with the default settings.
    pub const STOP_TIMEOUT: Duration = Duration::from_secs(180);

    /// Reads the batcher runtime state via `admin_getBatcherStatus`.
    pub async fn status(rpc: &Url) -> Result<BatcherStatus, BatcherCommandError> {
        Self::call(rpc, "admin_getBatcherStatus", Self::RPC_TIMEOUT).await
    }

    /// Stops batch submission via `admin_stopBatcher`.
    pub async fn stop(rpc: &Url) -> Result<(), BatcherCommandError> {
        Self::call(rpc, "admin_stopBatcher", Self::STOP_TIMEOUT).await
    }

    /// Starts batch submission again via `admin_startBatcher`.
    pub async fn start(rpc: &Url) -> Result<(), BatcherCommandError> {
        Self::call(rpc, "admin_startBatcher", Self::RPC_TIMEOUT).await
    }

    /// Closes the current channel via `admin_flushBatcher`.
    pub async fn flush(rpc: &Url) -> Result<(), BatcherCommandError> {
        Self::call(rpc, "admin_flushBatcher", Self::RPC_TIMEOUT).await
    }

    /// Origin-only URL for output, logs and errors so credentials never leak.
    pub fn display_url(rpc: &Url) -> String {
        rpc.origin().ascii_serialization()
    }

    async fn call<T: DeserializeOwned>(
        rpc: &Url,
        method: &'static str,
        timeout: Duration,
    ) -> Result<T, BatcherCommandError> {
        let client =
            HttpClientBuilder::default().request_timeout(timeout).build(rpc.as_str()).map_err(
                |source| BatcherCommandError::BuildClient { rpc: Self::display_url(rpc), source },
            )?;

        ClientT::request(&client, method, rpc_params![]).await.map_err(|source| match source {
            JsonRpcClientError::Call(rejection) => BatcherCommandError::Rejected {
                rpc: Self::display_url(rpc),
                method,
                message: rejection.message().to_string(),
            },
            source => BatcherCommandError::Rpc { rpc: Self::display_url(rpc), method, source },
        })
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::{
        server::{RpcModule, Server},
        types::ErrorObjectOwned,
    };

    use super::*;

    #[tokio::test]
    async fn calls_decode_answers_and_report_rejections() {
        let mut module = RpcModule::new(());
        module
            .register_method("admin_getBatcherStatus", |_, _, _| {
                serde_json::json!({ "stopped": true, "in_flight": 0, "da_backlog_bytes": 7 })
            })
            .unwrap();
        module.register_method("admin_stopBatcher", |_, _, _| ()).unwrap();
        module
            .register_method("admin_flushBatcher", |_, _, _| {
                Err::<(), _>(ErrorObjectOwned::owned(-32002, "batcher is stopped", None::<()>))
            })
            .unwrap();
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let handle = server.start(module);
        let rpc = Url::parse(&format!("http://operator:secret@{address}")).unwrap();

        let status = BatcherClient::status(&rpc).await.unwrap();
        assert_eq!(status, BatcherStatus { stopped: true, in_flight: 0, da_backlog_bytes: 7 });

        BatcherClient::stop(&rpc).await.unwrap();

        let error = BatcherClient::flush(&rpc).await.unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "batcher at http://{address} rejected `admin_flushBatcher`: batcher is stopped"
            )
        );

        handle.stop().unwrap();
    }

    #[test]
    fn display_url_drops_credentials_path_and_query() {
        let rpc = Url::parse("https://operator:secret@batcher.example:6545/admin?key=abc").unwrap();

        assert_eq!(BatcherClient::display_url(&rpc), "https://batcher.example:6545");
    }

    #[test]
    fn status_deserializes_the_admin_response() {
        let status: BatcherStatus = serde_json::from_value(serde_json::json!({
            "stopped": true,
            "in_flight": 2,
            "da_backlog_bytes": 1024,
        }))
        .unwrap();

        assert_eq!(status, BatcherStatus { stopped: true, in_flight: 2, da_backlog_bytes: 1024 });
    }
}
