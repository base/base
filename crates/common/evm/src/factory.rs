use alloy_evm::{Database, EvmEnv, EvmFactory, precompiles::PrecompilesMap};
use alloy_primitives::Address;
use revm::{
    Context, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use crate::{
    BaseContext, BaseEvm, BaseHaltReason, BaseSpecId, BaseTransaction, BaseTransactionError,
    Builder, DefaultBase,
};

/// Factory that produces [`BaseEvm`] instances backed by a [`PrecompilesMap`].
///
/// Base precompiles are eagerly flattened into a [`PrecompilesMap`] on construction so that
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

impl EvmFactory for BaseEvmFactory {
    type Evm<DB: Database, I: Inspector<BaseContext<DB>>> = BaseEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = BaseContext<DB>;
    type Tx = BaseTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, BaseTransactionError>;
    type HaltReason = BaseHaltReason;
    type Spec = BaseSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector_and_activation_admin_address(
                NoOpInspector {},
                self.activation_admin_address,
            )
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
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
