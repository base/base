use alloy_primitives::Address;
use base_execution_evm_runtime::{
    BaseContext, BaseEvm, Builder, Context, Database, DatabaseCommit, DefaultBase, EvmEnv,
    Inspector, NoOpInspector, TxTracer,
};

/// Factory that produces [`BaseEvm`] instances backed by a [`crate::PrecompilesMap`].
///
/// Base precompiles are eagerly flattened into a [`crate::PrecompilesMap`] on construction so that
/// precompile dispatch is a single hash-map lookup rather than a spec-aware branch on every call.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BaseEvmFactory {
    /// Activation registry admin address.
    activation_admin_address: Option<Address>,
}

impl BaseEvmFactory {
    /// Creates a new [`BaseEvmFactory`] with the given activation registry admin address.
    pub const fn new(activation_admin_address: Option<Address>) -> Self {
        Self { activation_admin_address }
    }

    /// Returns the activation registry admin address.
    pub const fn activation_admin_address(&self) -> Option<Address> {
        self.activation_admin_address
    }

    /// Returns this factory with the activation registry admin address set.
    #[must_use]
    pub const fn with_activation_admin_address(
        mut self,
        activation_admin_address: Option<Address>,
    ) -> Self {
        self.set_activation_admin_address(activation_admin_address);
        self
    }

    /// Sets the activation registry admin address.
    pub const fn set_activation_admin_address(
        &mut self,
        activation_admin_address: Option<Address>,
    ) {
        self.activation_admin_address = activation_admin_address;
    }
}

impl Default for BaseEvmFactory {
    fn default() -> Self {
        Self::new(None)
    }
}

impl BaseEvmFactory {
    /// Creates a Base EVM with the supplied database and environment.
    pub fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> BaseEvm<DB, NoOpInspector> {
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_base_with_activation_admin_address(self.activation_admin_address)
            .with_inspector(NoOpInspector {})
    }

    /// Creates a Base EVM using the supplied inspector.
    pub fn create_evm_with_inspector<DB: Database, I: Inspector<BaseContext<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> BaseEvm<DB, I> {
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector_and_activation_admin_address(
                inspector,
                self.activation_admin_address,
            )
    }
}

impl BaseEvmFactory {
    /// Creates a transaction tracer with the supplied database and inspector.
    pub fn create_tracer<DB, I>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> TxTracer<BaseEvm<DB, I>>
    where
        DB: Database + DatabaseCommit,
        I: Inspector<BaseContext<DB>> + Clone,
    {
        TxTracer::new(self.create_evm_with_inspector(db, input, inspector))
    }
}

#[cfg(test)]
mod tests {
    use base_execution_evm_runtime::{
        BaseSpecId, BaseUpgrade, BlockEnv, CfgEnv, EmptyDB, EvmEnv, NoOpInspector,
    };

    use super::*;

    fn default_env() -> EvmEnv {
        EvmEnv::new(CfgEnv::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)), BlockEnv::default())
    }

    #[test]
    pub fn create_evm_has_inspect_false() {
        let factory = BaseEvmFactory::default();
        let evm = factory.create_evm(EmptyDB::default(), default_env());
        assert!(!evm.inspect);
    }

    #[test]
    pub fn create_evm_with_inspector_has_inspect_true() {
        let factory = BaseEvmFactory::default();
        let evm =
            factory.create_evm_with_inspector(EmptyDB::default(), default_env(), NoOpInspector {});
        assert!(evm.inspect);
    }
}
