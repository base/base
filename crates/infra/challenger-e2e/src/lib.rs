#![doc = include_str!("../README.md")]

mod config;
pub use config::Config;

mod metrics;
pub use metrics::Scrape;
