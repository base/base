//! This is our custom implementation of validator struct

use jsonrpsee::http_client::HttpClient;
use kona_rpc::InteropTxValidator;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SupervisorValidator {
    inner: HttpClient,
}

impl SupervisorValidator {
    pub fn new(client: HttpClient) -> Self {
        Self { inner: client }
    }
}

impl InteropTxValidator for SupervisorValidator {
    type SupervisorClient = HttpClient;
    const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);

    fn supervisor_client(&self) -> &Self::SupervisorClient {
        &self.inner
    }
}
