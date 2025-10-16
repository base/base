use reqwest::Client;
use tracing::info;

const DEBUG_QUOTE_SERVICE_URL: &str = "http://ns31695324.ip-141-94-163.eu:10080/attest";

/// Configuration for attestation
#[derive(Default)]
pub struct AttestationConfig {
    /// If true, uses the debug HTTP service instead of real TDX hardware
    pub debug: bool,
    /// The URL of the quote provider
    pub quote_provider: Option<String>,
}
/// Remote attestation provider
#[derive(Debug, Clone)]
pub struct RemoteAttestationProvider {
    client: Client,
    service_url: String,
}

impl RemoteAttestationProvider {
    pub fn new(service_url: String) -> Self {
        let client = Client::new();
        Self {
            client,
            service_url,
        }
    }
}

impl RemoteAttestationProvider {
    pub async fn get_attestation(&self, report_data: [u8; 64]) -> eyre::Result<Vec<u8>> {
        let report_data_hex = hex::encode(report_data);
        let url = format!("{}/{}", self.service_url, report_data_hex);

        info!(target: "flashtestations", url = url, "fetching quote from remote attestation provider");

        let response = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?;
        let body = response.bytes().await?.to_vec();

        Ok(body)
    }
}

pub fn get_attestation_provider(config: AttestationConfig) -> RemoteAttestationProvider {
    if config.debug {
        RemoteAttestationProvider::new(
            config
                .quote_provider
                .unwrap_or(DEBUG_QUOTE_SERVICE_URL.to_string()),
        )
    } else {
        RemoteAttestationProvider::new(
            config
                .quote_provider
                .expect("remote quote provider must be specified when not in debug mode"),
        )
    }
}
