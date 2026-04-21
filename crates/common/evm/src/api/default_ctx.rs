//! Contains trait [`DefaultBase`] used to create a default context.
use revm::{
    Context, Journal, MainContext,
    context::{BlockEnv, CfgEnv, TxEnv},
    database_interface::EmptyDB,
};

use crate::{L1BlockInfo, OpSpecId, BaseTransaction};

/// Type alias for the default context type of the `OpEvm`.
pub type BaseContext<DB> =
    Context<BlockEnv, BaseTransaction<TxEnv>, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Trait that allows for a default context to be created.
pub trait DefaultBase {
    /// Create a default context.
    fn op() -> BaseContext<EmptyDB>;
}

impl DefaultBase for BaseContext<EmptyDB> {
    fn op() -> Self {
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
    fn default_run_op() {
        let ctx = Context::op();
        let mut evm = ctx.build_with_inspector(NoOpInspector {});
        // execute without inspector
        let _ = evm.transact(BaseTransaction::builder().build_fill());
        // execute with inspector callbacks
        let _ = evm.inspect_one_tx(BaseTransaction::builder().build_fill());
    }
}
