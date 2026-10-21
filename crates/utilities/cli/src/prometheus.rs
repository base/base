//! Utilities for spinning up a prometheus metrics server.

use std::{
    net::{IpAddr, SocketAddr},
    thread::{self, sleep},
    time::Duration,
};

pub use metrics_exporter_prometheus::BuildError;
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use tracing::info;

/// Complete metrics configuration.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Whether metrics are enabled.
    pub enabled: bool,
    /// Metrics collection interval in seconds.
    pub interval: u64,
    /// Metrics server bind address.
    pub addr: IpAddr,
    /// Metrics server port.
    pub port: u16,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: 30,
            addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port: 9090,
        }
    }
}

impl MetricsConfig {
    /// Initialize the Prometheus metrics recorder.
    pub fn init(&self) -> Result<(), BuildError> {
        self.init_with_builder(Ok)
    }

    /// Initialize the Prometheus metrics recorder with a customized builder.
    pub fn init_with_builder<F>(&self, configure: F) -> Result<(), BuildError>
    where
        F: FnOnce(PrometheusBuilder) -> Result<PrometheusBuilder, BuildError>,
    {
        if !self.enabled {
            return Ok(());
        }

        let prometheus_addr = SocketAddr::from((self.addr, self.port));
        let builder = configure(PrometheusBuilder::new().with_http_listener(prometheus_addr))?;

        builder.install()?;
        base_metrics::initialize_registered_metrics();

        // Initialise collector for system metrics e.g. CPU, memory, etc.
        let collector = Collector::default();
        collector.describe();

        let interval = self.interval;
        thread::spawn(move || {
            loop {
                collector.collect();
                sleep(Duration::from_secs(interval));
            }
        });

        info!(
            target: "prometheus",
            addr = %prometheus_addr,
            "Serving metrics"
        );

        Ok(())
    }

    /// Initialize the Prometheus metrics recorder and run a callback to initialize
    /// subsystem-specific metrics if metrics are enabled.
    pub fn init_with<F>(&self, f: F) -> Result<(), BuildError>
    where
        F: FnOnce(),
    {
        if self.enabled {
            self.init()?;
            f();
        }
        Ok(())
    }
}
