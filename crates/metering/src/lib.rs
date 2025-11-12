//! # Base Reth Metering
//!
//! This crate provides metering functionality for the Base Reth node, enabling detailed
//! measurement and analysis of transaction bundle execution.
//!
//! ## Overview
//!
//! The metering crate is designed to simulate and measure the gas consumption and execution
//! time of transaction bundles. This is particularly useful for:
//!
//! - Analyzing bundle efficiency before submission
//! - Measuring gas usage across multiple transactions
//! - Understanding execution performance characteristics
//! - Validating bundle behavior in a simulated environment
//!
//! ## Main Components
//!
//! - **`meter_bundle`**: Core function that simulates and meters transaction bundles
//! - **`MeteringApiServer`**: JSON-RPC API server interface for metering operations
//! - **`MeteringApiImpl`**: Implementation of the metering API
//!
//! ## Usage
//!
//! The metering functionality can be accessed via the JSON-RPC API or by directly calling
//! the `meter_bundle` function. Bundles follow the TIPS (Transaction Inclusion Protocol
//! Specification) format.
//!
//! ## Example
//!
//! ```rust,ignore
//! use base_reth_metering::{meter_bundle, Bundle};
//!
//! // Create a bundle with transactions
//! let bundle = Bundle {
//!     block_number: 1000,
//!     transactions: vec![/* transaction data */],
//!     // ... other fields
//! };
//!
//! // Meter the bundle
//! let response = meter_bundle(provider, bundle).await?;
//! println!("Total gas used: {}", response.total_gas_used);
//! ```

mod meter;
mod rpc;
#[cfg(test)]
mod tests;

pub use meter::meter_bundle;
pub use rpc::{MeteringApiImpl, MeteringApiServer};
pub use tips_core::types::{Bundle, MeterBundleResponse, TransactionResult};
