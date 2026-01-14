//! CLI argument definitions for the Base builder.
//!
//! This crate contains CLI argument structs used by the op-rbuilder and related components.
//! It provides a separation of concerns by extracting CLI-related code into a dedicated crate.

#![doc(issue_tracker_base_url = "https://github.com/base/node-reth/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod flashblocks;
mod gas_limiter;
mod op;
mod telemetry;

pub use flashblocks::FlashblocksArgs;
pub use gas_limiter::GasLimiterArgs;
pub use op::OpRbuilderArgs;
pub use telemetry::TelemetryArgs;
