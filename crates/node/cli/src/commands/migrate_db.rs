//! Migrate storage from v1 to v2 format.

use std::sync::Arc;

use base_alloy_consensus::OpPrimitives;
use base_common_chain_config::{BaseChainSpec, OpChainSpec};
use clap::Parser;

/// Migrate storage from v1 (MDBX-only) to v2 (MDBX + `RocksDB` + static files).
#[derive(Debug, Parser)]
pub struct Command {
    #[command(flatten)]
    inner: base_migrate_db::Command,
}

impl Command {
    /// Executes the migration command.
    pub async fn execute(self, runtime: base_common_runtime::Runtime) -> eyre::Result<()> {
        self.inner.execute(runtime).await
    }
}

impl Command {
    /// Returns the chain spec, if configured.
    pub const fn chain_spec(&self) -> Option<&Arc<BaseChainSpec>> {
        self.inner.chain_spec()
    }
}
