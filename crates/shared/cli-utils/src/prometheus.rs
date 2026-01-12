//! Utilities for spinning up a prometheus metrics server.

use std::{
    net::{IpAddr, SocketAddr},
    thread::{self, sleep},
    time::Duration,
};

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder};
use metrics_process::Collector;
use tracing::info;

/// A wrapper type that initializes a Prometheus metrics server.
#[derive(Debug, Clone, Copy)]
pub struct PrometheusServer;

impl PrometheusServer {
    /// Initialize a Prometheus metrics server on the given address and port.
    pub fn init(addr: IpAddr, metrics_port: u16) -> Result<(), BuildError> {
        let prometheus_addr = SocketAddr::from((addr, metrics_port));
        let builder = PrometheusBuilder::new().with_http_listener(prometheus_addr);

        builder.install()?;

        // Initialise collector for system metrics e.g. CPU, memory, etc.
        let collector = Collector::default();
        collector.describe();

        thread::spawn(move || {
            loop {
                collector.collect();
                sleep(Duration::from_secs(60));
            }
        });

        info!(
            target: "prometheus",
            "Serving metrics at: http://{}",
            prometheus_addr
        );

        Ok(())
    }
}
