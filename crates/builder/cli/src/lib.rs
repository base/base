#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use reth_optimism_cli::chainspec::OpChainSpecParser;

mod flashblocks;
pub use flashblocks::FlashblocksArgs;

mod op;
pub use op::OpRbuilderArgs;

mod telemetry;
pub use telemetry::TelemetryArgs;

/// CLI type alias for the OP builder.
pub type Cli = reth_optimism_cli::Cli<OpChainSpecParser, OpRbuilderArgs>;
