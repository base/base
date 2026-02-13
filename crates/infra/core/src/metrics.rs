use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Initializes and installs a Prometheus metrics exporter listening on the given address.
pub fn init_prometheus_exporter(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}
