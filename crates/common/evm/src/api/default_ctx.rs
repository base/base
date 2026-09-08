//! Contains trait [`DefaultBase`] used to create a default context.
use base_common_genesis::BaseUpgrade;
use base_evm_context::{BlockEnv, CfgEnv};
use revm::{Context, Journal, MainContext, database_interface::EmptyDB};

use crate::{BaseSpecId, BaseTransaction, L1BlockInfo};

/// Type alias for the default context type of the `BaseEvm`.
pub type BaseContext<DB> =
    Context<BlockEnv, BaseTransaction, CfgEnv<BaseSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Trait that allows for a default context to be created.
pub trait DefaultBase {
    /// Create a default context.
    fn base() -> BaseContext<EmptyDB>;
}

impl DefaultBase for BaseContext<EmptyDB> {
    fn base() -> Self {
        Context::mainnet()
            .with_tx(BaseTransaction::builder().build_fill())
            .with_cfg(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Bedrock)))
            .with_chain(L1BlockInfo::default())
    }
}

#[cfg(test)]
mod tests {
    use base_evm_handler::NoOpInspector;
    use revm::{ExecuteEvm, InspectEvm};

    use super::*;
    use crate::Builder;

    #[test]
    fn default_run_base() {
        let ctx = Context::base();
        let mut evm = ctx.build_with_inspector(NoOpInspector {});
        // execute without inspector
        let _ = evm.transact(BaseTransaction::builder().build_fill());
        // execute with inspector callbacks
        let _ = evm.inspect_one_tx(BaseTransaction::builder().build_fill());
    }
}
