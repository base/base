//! Contains trait [`DefaultBase`] used to create a default context.
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
};

use crate::{BaseTransaction, L1BlockInfo, OpSpecId};

/// Type alias for the default context type of the `BaseEvm`.
pub type BaseContext<DB> =
    Context<BlockEnv, BaseTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Trait that allows for a default context to be created.
pub trait DefaultBase {
    /// Create a default context.
    fn base() -> BaseContext<EmptyDB>;
}

impl DefaultBase for BaseContext<EmptyDB> {
    fn base() -> Self {
        Context::mainnet()
            .with_tx(BaseTransaction::builder().build_fill())
            .with_cfg(CfgEnv::new_with_spec(OpSpecId::BEDROCK))
            .with_chain(L1BlockInfo::default())
    }
}

#[cfg(test)]
mod tests {
    use revm::{ExecuteEvm, InspectEvm, inspector::NoOpInspector};

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
