use std::io::Read;
use tracing::info;
use ureq;

const DEBUG_QUOTE_SERVICE_URL: &str = "http://ns31695324.ip-141-94-163.eu:10080/attest";

/// Configuration for attestation
#[derive(Default)]
pub struct AttestationConfig {
    /// If true, uses the debug HTTP service instead of real TDX hardware
    pub debug: bool,
    /// The URL of the quote provider
    pub quote_provider: Option<String>,
}

/// Trait for attestation providers
pub trait AttestationProvider {
    fn get_attestation(&self, report_data: [u8; 64]) -> eyre::Result<Vec<u8>>;
}

/// Remote attestation provider
pub struct RemoteAttestationProvider {
    service_url: String,
}

impl RemoteAttestationProvider {
    pub fn new(service_url: String) -> Self {
        Self { service_url }
    }
}

impl AttestationProvider for RemoteAttestationProvider {
    fn get_attestation(&self, report_data: [u8; 64]) -> eyre::Result<Vec<u8>> {
        let report_data_hex = hex::encode(report_data);
        let url = format!("{}/{}", self.service_url, report_data_hex);

        info!(target: "flashtestations", url = url, "fetching quote in debug mode");

        let response = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .call()?;

        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body)?;

        Ok(body)
    }
}

pub fn get_attestation_provider(
    config: AttestationConfig,
) -> Box<dyn AttestationProvider + Send + Sync> {
    if config.debug {
        Box::new(RemoteAttestationProvider::new(
            config
                .quote_provider
                .unwrap_or(DEBUG_QUOTE_SERVICE_URL.to_string()),
        ))
    } else {
        Box::new(RemoteAttestationProvider::new(
            config
                .quote_provider
                .expect("remote quote provider must be specified when not in debug mode"),
        ))
    }
}
