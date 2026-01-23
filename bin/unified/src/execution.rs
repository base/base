//! Execution client wrapper.

use base_client_node::{BaseNodeBuilder, BaseNodeHandle, BaseNodeRunner};
use reth_optimism_node::args::RollupArgs;

/// Wrapper around the reth execution client.
#[derive(Debug, Clone)]
pub struct Execution {
    rollup_args: RollupArgs,
}

impl Execution {
    /// Creates a new [`Execution`] instance.
    pub const fn new(rollup_args: RollupArgs) -> Self {
        Self { rollup_args }
    }

    /// Runs the execution client, returning a [`BaseNodeHandle`].
    pub fn run(self, builder: BaseNodeBuilder) -> BaseNodeHandle {
        let runner = BaseNodeRunner::new(self.rollup_args);
        runner.run(builder)
    }
}
