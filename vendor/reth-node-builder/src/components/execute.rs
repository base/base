//! EVM component for the node builder.
use std::future::Future;

use reth_evm::BaseEvmConfig;

use crate::{BuilderContext, FullNodeTypes};

/// A type that knows how to build the executor types.
pub trait ExecutorBuilder<Node: FullNodeTypes>: Send {
    /// Creates the EVM config.
    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<BaseEvmConfig>> + Send;
}

impl<Node, F, Fut> ExecutorBuilder<Node> for F
where
    Node: FullNodeTypes,
    F: FnOnce(&BuilderContext<Node>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<BaseEvmConfig>> + Send,
{
    fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<BaseEvmConfig>> {
        self(ctx)
    }
}
