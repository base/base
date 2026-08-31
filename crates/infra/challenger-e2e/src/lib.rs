//! Behavioural end-to-end test of the challenger against an ephemeral L1 fork.
//!
//! This crate currently carries the driver's configuration and its Prometheus
//! scraper. The driver itself lands separately.

mod config;
pub use config::Config;

mod metrics;
pub use metrics::Scrape;
