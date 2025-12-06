//! Utilities for spinning up local Optimism nodes, engines, and helpers used in
//! integration tests across the Base codebase.

/// Convenience types and helpers for working with deterministic test accounts.
pub mod accounts;
/// Shared fixtures and test data reused by integration tests.
pub mod fixtures;
/// Ergonomic wrapper around the Engine API clients used by the harness.
pub mod engine;
/// Flashblocks-aware harness helpers.
pub mod flashblocks_harness;
/// High-level façade for interacting with a local Base node.
pub mod harness;
/// Local node construction, flashblocks plumbing, and launchers.
pub mod node;
/// Lightweight tracing initialization helpers for tests.
pub mod tracing;
