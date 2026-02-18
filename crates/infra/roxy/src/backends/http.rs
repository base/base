//! Contains the [`HttpBackend`] struct, an HTTP-based backend implementation for Roxy.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use async_trait::async_trait;

use super::{BackendConfig, EmaHealthTracker, HealthTracker};
use crate::{HealthStatus, RoxyError};

/// HTTP-based RPC backend.
#[derive(Debug)]
pub struct HttpBackend {
    /// Backend name identifier.
    pub name: String,
    /// RPC endpoint URL.
    pub rpc_url: String,
    /// HTTP client.
    pub client: reqwest::Client,
    /// Health tracker.
    pub health: Arc<RwLock<EmaHealthTracker>>,
    /// Backend configuration.
    pub config: BackendConfig,
}

impl HttpBackend {
    /// Create a new HTTP backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client fails to build.
    pub fn new(name: String, rpc_url: String, config: BackendConfig) -> Result<Self, RoxyError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| RoxyError::Internal(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            name,
            rpc_url,
            client,
            health: Arc::new(RwLock::new(EmaHealthTracker::new(Default::default()))),
            config,
        })
    }

    async fn do_forward(&self, request: &RequestPacket) -> Result<ResponsePacket, RoxyError> {
        let start = Instant::now();

        let response = self
            .client
            .post(&self.rpc_url)
            .json(request)
            .send()
            .await
            .map_err(|_| RoxyError::BackendOffline { backend: self.name.clone() })?;

        let duration = start.elapsed();
        let success = response.status().is_success();

        if let Ok(mut health) = self.health.write() {
            health.record(duration, success);
        }

        if !success {
            return Err(RoxyError::BackendOffline { backend: self.name.clone() });
        }

        response
            .json()
            .await
            .map_err(|e| RoxyError::Internal(format!("failed to parse response: {e}")))
    }
}

#[async_trait]
impl super::Backend for HttpBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    async fn forward(&self, request: RequestPacket) -> Result<ResponsePacket, RoxyError> {
        let mut last_error = None;

        for attempt in 0..=self.config.max_retries {
            match self.do_forward(&request).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.config.max_retries {
                        let backoff = Duration::from_millis((2u64.pow(attempt) * 100).min(3000));
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        // SAFETY: Loop always sets last_error before exiting without return
        Err(last_error.expect("loop guarantees error was set"))
    }

    fn health_status(&self) -> HealthStatus {
        self.health.read().map(|h| h.status()).unwrap_or(HealthStatus::Healthy)
    }

    fn latency_ema(&self) -> Duration {
        self.health.read().map(|h| h.latency_ema()).unwrap_or(Duration::ZERO)
    }
}
