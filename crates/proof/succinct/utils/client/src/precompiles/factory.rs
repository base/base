//! [`EvmFactory`] implementation for the EVM in the ZKVM environment.

use alloy_evm::{Database, EvmEnv, EvmFactory};
use alloy_primitives::Address;
use base_common_evm::{
    BaseContext, BaseEvm, BaseHaltReason, BaseSpecId, BaseTransaction, BaseTransactionError,
    Builder, DefaultBase,
};
use revm::{
    Context, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use super::BaseZkvmPrecompiles;

/// Factory producing [`BaseEvm`]s with ZKVM-accelerated precompile overrides enabled.
#[derive(Debug, Clone, Copy)]
pub struct ZkvmBaseEvmFactory {
    /// Activation registry admin address.
    activation_admin_address: Option<Address>,
}

impl ZkvmBaseEvmFactory {
    /// Creates a new [`ZkvmBaseEvmFactory`].
    pub const fn new() -> Self {
        Self::new_with_activation_admin_address(None)
    }

    /// Creates a new [`ZkvmBaseEvmFactory`] with the given activation registry admin address.
    pub const fn new_with_activation_admin_address(
        activation_admin_address: Option<Address>,
    ) -> Self {
        Self { activation_admin_address }
    }

    /// Returns the activation registry admin address.
    pub const fn activation_admin_address(&self) -> Option<Address> {
        self.activation_admin_address
    }
}

impl Default for ZkvmBaseEvmFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl EvmFactory for ZkvmBaseEvmFactory {
    type Evm<DB: Database, I: Inspector<BaseContext<DB>>> = BaseEvm<DB, I, BaseZkvmPrecompiles>;
    type Context<DB: Database> = BaseContext<DB>;
    type Tx = BaseTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, BaseTransactionError>;
    type HaltReason = BaseHaltReason;
    type Spec = BaseSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = BaseZkvmPrecompiles;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        let precompiles = BaseZkvmPrecompiles::new_with_spec_and_activation_admin_address(
            spec_id,
            self.activation_admin_address,
        );
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector_and_precompiles(NoOpInspector {}, precompiles)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<BaseSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let precompiles = BaseZkvmPrecompiles::new_with_spec_and_activation_admin_address(
            spec_id,
            self.activation_admin_address,
        );
        Context::base()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_with_inspector_and_precompiles(inspector, precompiles)
    }
}
