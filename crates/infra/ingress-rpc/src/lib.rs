#![doc = include_str!("../README.md")]

/// Forwarding of ingress metering data to builder RPCs.
mod builder;
pub use builder::{BuilderConnector, MeteringForwardMessage};

/// Configuration for the tips ingress RPC service.
mod config;
pub use config::Config;

/// Health check HTTP server.
mod health;
pub use health::HealthServer;

/// Prometheus metrics for the ingress RPC service.
mod metrics;
pub use metrics::Metrics;

/// Core RPC service implementation.
mod service;
pub use service::{IngressApiServer, IngressService};

/// Transaction validation implementation.
mod validation;
pub use validation::{AccountInfo, AccountInfoLookup, L1BlockInfoLookup, validate_bundle};
