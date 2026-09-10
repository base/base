//! Construction of Base block executors.

use base_common_chain_config::ChainConfig;
use base_execution_evm_runtime::{
    BaseBlockExecutionCtx, BaseBlockExecutor, BaseContext, BaseEvm, BaseEvmFactory, Inspector,
    StateDB,
};

/// Factory for the Base execution configuration.
#[derive(Debug, Clone, Default)]
pub struct BaseBlockExecutorFactory {
    /// Execution configuration, including runtime upgrade activation.
    pub spec: ChainConfig,
    /// Factory for Base EVM instances.
    pub evm_factory: BaseEvmFactory,
}

impl BaseBlockExecutorFactory {
    /// Creates a factory using normalized Base chain configuration.
    pub const fn new(spec: ChainConfig, evm_factory: BaseEvmFactory) -> Self {
        Self { spec, evm_factory }
    }

    /// Creates an executor for the supplied state and block context.
    pub fn create_executor<DB, I>(
        &self,
        evm: BaseEvm<DB, I>,
        ctx: BaseBlockExecutionCtx,
    ) -> BaseBlockExecutor<DB, I>
    where
        DB: StateDB,
        I: Inspector<BaseContext<DB>>,
    {
        BaseBlockExecutor::new(evm, ctx, self.spec.clone())
    }
}
