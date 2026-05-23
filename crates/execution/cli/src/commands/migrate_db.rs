//! Migrate storage from v1 to v2 format.

use std::sync::Arc;

use base_alloy_consensus::OpPrimitives;
use base_execution_chainspec::OpChainSpec;
use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::CliNodeTypes;

/// Migrate storage from v1 (MDBX-only) to v2 (MDBX + `RocksDB` + static files).
#[derive(Debug, Parser)]
pub struct Command<C: ChainSpecParser> {
    #[command(flatten)]
    inner: base_migrate_db::Command<C>,
}

impl<C: ChainSpecParser<ChainSpec = OpChainSpec>> Command<C> {
    /// Executes the migration command.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = OpPrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        self.inner.execute::<N>(runtime).await
    }
}

impl<C: ChainSpecParser> Command<C> {
    /// Returns the chain spec, if configured.
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        self.inner.chain_spec()
    }
}
