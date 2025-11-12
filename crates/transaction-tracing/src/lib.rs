//! # Base Reth Transaction Tracing
//!
//! This crate provides transaction tracing capabilities for the Base Reth node through
//! an Execution Extension (ExEx).
//!
//! ## Overview
//!
//! Transaction tracing enables detailed monitoring and analysis of transaction execution
//! within the Base L2 network. This is crucial for:
//!
//! - Debugging transaction execution issues
//! - Analyzing transaction behavior and state changes
//! - Monitoring network activity and patterns
//! - Building analytical tools and dashboards
//!
//! ## Architecture
//!
//! The tracing functionality is implemented as a Reth Execution Extension (ExEx), which
//! allows it to hook into the node's execution pipeline and observe transactions as they
//! are processed.
//!
//! ## Main Components
//!
//! - **`transaction_tracing_exex`**: The main ExEx function that processes execution notifications
//! - **Tracing types**: Data structures representing traced transaction information
//!
//! ## Usage
//!
//! The transaction tracing ExEx is typically initialized during node startup and runs
//! continuously alongside the node, processing each committed chain event.
//!
//! ## Example
//!
//! ```rust,ignore
//! use base_reth_transaction_tracing::transaction_tracing_exex;
//! use reth_exex::ExExContext;
//!
//! async fn init_tracing(ctx: ExExContext) -> eyre::Result<()> {
//!     transaction_tracing_exex(ctx).await
//! }
//! ```

pub mod tracing;
mod types;

pub use tracing::transaction_tracing_exex;
