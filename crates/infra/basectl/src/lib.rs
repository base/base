#![doc = include_str!("../README.md")]

mod app;
pub use app::{ViewId, run_app, run_app_with_view};

mod commands;
mod config;
pub use config::ChainConfig;

mod l1_client;
mod rpc;
mod tui;
