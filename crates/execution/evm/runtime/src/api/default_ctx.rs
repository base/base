//! Contains trait [`DefaultBase`] used to create a default context.
use base_common_chain_config::BaseUpgrade;
use base_execution_evm_runtime::{BaseSpecId, BaseTransaction, CfgEnv, EmptyDB, L1BlockInfo};

/// Type alias for the default context type of the `BaseEvm`.
pub use crate::Context as BaseContext;

/// Trait that allows for a default context to be created.
pub trait DefaultBase {
    /// Create a default context.
    fn base() -> BaseContext<EmptyDB>;
}

impl DefaultBase for BaseContext<EmptyDB> {
    fn base() -> Self {
        Self::new(EmptyDB::default(), BaseSpecId::new(BaseUpgrade::Bedrock))
            .with_tx(BaseTransaction::builder().build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Bedrock)))
            .with_chain(L1BlockInfo::default())
    }
}

#[cfg(test)]
mod tests {
    use base_execution_evm_runtime::{Builder, ExecuteEvm, InspectEvm, NoOpInspector};

    use super::*;

    #[test]
    fn default_run_base() {
        let ctx = BaseContext::base();
        let mut evm = ctx.build_with_inspector(NoOpInspector {});
        // execute without inspector
        let _ = evm.transact(BaseTransaction::builder().build_fill());
        // execute with inspector callbacks
        let _ = evm.inspect_one_tx(BaseTransaction::builder().build_fill());
    }
}
