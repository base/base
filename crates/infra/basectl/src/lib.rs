#![doc = include_str!("../README.md")]

/// TUI application framework.
pub mod app;
/// CLI commands.
pub mod commands;
/// Chain configuration.
pub mod config;
/// L1 Ethereum client.
pub mod l1_client;
/// RPC client utilities.
pub mod rpc;
/// Terminal UI rendering.
pub mod tui;
