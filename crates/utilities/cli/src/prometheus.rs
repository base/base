//! Utilities for spinning up a prometheus metrics server.

use std::{
    any::Any,
    net::{IpAddr, SocketAddr},
    panic::{self, AssertUnwindSafe},
    thread::{self, sleep},
    time::Duration,
};

pub use metrics_exporter_prometheus::BuildError;
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_process::Collector;
use tracing::{error, info};

/// A wrapper type that initializes a Prometheus metrics server.
#[derive(Debug, Clone, Copy)]
pub struct PrometheusServer;

impl PrometheusServer {
    /// Initialize a Prometheus metrics server on the given address and port.
    ///
    /// Initialize a Prometheus metrics server on the given address and port.
    ///
    /// The interval specifies how often system metrics are collected, in seconds.
    pub fn init(addr: IpAddr, metrics_port: u16, interval: u64) -> Result<(), BuildError> {
        let prometheus_addr = SocketAddr::from((addr, metrics_port));
        let builder = PrometheusBuilder::new().with_http_listener(prometheus_addr);

        builder.install()?;

        // Initialise collector for system metrics e.g. CPU, memory, etc.
        let collector = Collector::default();
        collector.describe();

        thread::spawn(move || {
            loop {
                if let Err(panic_payload) =
                    panic::catch_unwind(AssertUnwindSafe(|| collector.collect()))
                {
                    error!(
                        target: "prometheus",
                        panic = %panic_message(panic_payload.as_ref()),
                        "metrics collector panicked, continuing collection loop"
                    );
                }
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
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(msg) = payload.downcast_ref::<&'static str>() {
        msg
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.as_str()
    } else {
        "non-string panic payload"
    }
}

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
        if self.enabled {
            PrometheusServer::init(self.addr, self.port, self.interval)?;
        }
        Ok(())
    }

    /// Initialize the Prometheus metrics recorder and run a callback to initialize
    /// subsystem-specific metrics if metrics are enabled.
    pub fn init_with<F>(&self, f: F) -> Result<(), BuildError>
    where
        F: FnOnce(),
    {
        if self.enabled {
            PrometheusServer::init(self.addr, self.port, self.interval)?;
            f();
        }
        Ok(())
    }
}
