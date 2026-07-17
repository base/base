//! Shadow-drive CLI flags.

use std::{num::ParseIntError, time::Duration};

use base_consensus_node::ShadowDriveConfig;
use clap::Parser;
use url::Url;

/// Shadow-drive CLI flags.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
pub struct ShadowDriveArgs {
    /// The L2 RPC URL for the shadow-drive source node.
    #[arg(
        long = "source",
        visible_alias = "shadow.source",
        default_value = "http://localhost:8545",
        env = "BASE_NODE_SHADOW_SOURCE_L2_RPC"
    )]
    pub source_l2_rpc: Url,

    /// Deadline for building shadow payloads, in milliseconds.
    #[arg(
        long = "shadow-build-deadline-ms",
        visible_alias = "shadow.build-deadline-ms",
        default_value = "0",
        value_parser = |arg: &str| -> Result<Duration, ParseIntError> {
            Ok(Duration::from_millis(arg.parse()?))
        },
        env = "BASE_NODE_SHADOW_BUILD_DEADLINE_MS"
    )]
    pub build_deadline: Duration,

    /// Maximum number of blocks to reorg when shadow-drive detects divergence.
    #[arg(
        long = "shadow-max-reorg-depth",
        visible_alias = "shadow.max-reorg-depth",
        default_value_t = 1,
        env = "BASE_NODE_SHADOW_MAX_REORG_DEPTH"
    )]
    pub max_reorg_depth: u64,
}

impl Default for ShadowDriveArgs {
    fn default() -> Self {
        Self::parse_from::<[_; 0], &str>([])
    }
}

impl ShadowDriveArgs {
    /// Creates a [`ShadowDriveConfig`] from the [`ShadowDriveArgs`].
    pub fn config(&self) -> ShadowDriveConfig {
        ShadowDriveConfig {
            source_l2_rpc: self.source_l2_rpc.clone(),
            build_deadline: self.build_deadline,
            max_reorg_depth: self.max_reorg_depth,
        }
    }
}
