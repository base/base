//! Privacy EVM Configuration
//!
//! This module provides an EVM factory that extends the standard OP precompiles
//! with privacy precompiles for contract registration and authorization.
//!
//! # Architecture
//!
//! Rather than wrapping `OpEvmConfig`, we provide a custom `EvmFactory` that:
//! 1. Uses `OpEvmFactory` as the base
//! 2. Extends the precompiles map with privacy precompiles (0x200, 0x201)
//!
//! This approach is simpler and more composable than wrapping the entire config.

use crate::precompiles::{
    auth, registry, AUTH_PRECOMPILE_ADDRESS, REGISTRY_PRECOMPILE_ADDRESS,
};
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv, EvmFactory};
use alloy_op_evm::{OpEvm, OpEvmFactory};
use op_revm::{precompiles::OpPrecompiles, DefaultOp, OpBuilder, OpContext, OpSpecId};
use revm::{
    inspector::NoOpInspector,
    precompile::PrecompileId,
    Inspector,
};

/// A custom EVM factory that extends OP precompiles with privacy precompiles.
///
/// This factory wraps `OpEvmFactory` and adds the privacy-specific precompiles
/// at addresses 0x200 (Registry) and 0x201 (Auth).
#[derive(Debug, Default, Clone, Copy)]
pub struct PrivacyEvmFactory;

impl PrivacyEvmFactory {
    /// Creates a new privacy EVM factory.
    pub const fn new() -> Self {
        Self
    }

    /// Extends the given precompiles map with privacy precompiles.
    fn extend_with_privacy_precompiles(precompiles: PrecompilesMap) -> PrecompilesMap {
        use alloy_evm::precompiles::DynPrecompile;

        // Create privacy precompiles as DynPrecompiles
        // We mark them as stateful because they access thread-local context
        let registry_precompile = DynPrecompile::new_stateful(
            PrecompileId::Custom("PrivacyRegistry".into()),
            |input| {
                registry::run(input.data, input.gas)
            },
        );

        let auth_precompile = DynPrecompile::new_stateful(
            PrecompileId::Custom("PrivacyAuth".into()),
            |input| {
                auth::run(input.data, input.gas)
            },
        );

        // Extend the base precompiles with our privacy precompiles
        precompiles.with_extended_precompiles([
            (REGISTRY_PRECOMPILE_ADDRESS, registry_precompile),
            (AUTH_PRECOMPILE_ADDRESS, auth_precompile),
        ])
    }
}

impl EvmFactory for PrivacyEvmFactory {
    type Evm<DB: Database, I: Inspector<OpContext<DB>>> = OpEvm<DB, I, PrecompilesMap>;
    type Context<DB: Database> = OpContext<DB>;
    type Tx = <OpEvmFactory as EvmFactory>::Tx;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        <OpEvmFactory as EvmFactory>::Error<DBError>;
    type HaltReason = <OpEvmFactory as EvmFactory>::HaltReason;
    type Spec = OpSpecId;
    type BlockEnv = <OpEvmFactory as EvmFactory>::BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        // Get the base OP precompiles for this spec
        let spec_id = input.cfg_env.spec;
        let base_precompiles = PrecompilesMap::from_static(
            OpPrecompiles::new_with_spec(spec_id).precompiles()
        );

        // Extend with privacy precompiles
        let precompiles = Self::extend_with_privacy_precompiles(base_precompiles);

        // Build the inner op_revm::OpEvm with our extended precompiles
        let inner = revm::Context::op()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_op_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles);

        OpEvm::new(inner, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        // Get the base OP precompiles for this spec
        let spec_id = input.cfg_env.spec;
        let base_precompiles = PrecompilesMap::from_static(
            OpPrecompiles::new_with_spec(spec_id).precompiles()
        );

        // Extend with privacy precompiles
        let precompiles = Self::extend_with_privacy_precompiles(base_precompiles);

        // Build the inner op_revm::OpEvm with our extended precompiles and inspector
        let inner = revm::Context::op()
            .with_db(db)
            .with_block(input.block_env)
            .with_cfg(input.cfg_env)
            .build_op_with_inspector(inspector)
            .with_precompiles(precompiles);

        OpEvm::new(inner, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::Evm as EvmTrait;
    use alloy_primitives::Address;
    use revm::{context::CfgEnv, database::EmptyDB};

    #[test]
    fn test_privacy_evm_factory_creates_evm() {
        let factory = PrivacyEvmFactory::new();
        let db = EmptyDB::default();
        let evm_env = EvmEnv::new(
            CfgEnv::new_with_spec(OpSpecId::CANYON),
            Default::default(),
        );

        let evm = factory.create_evm(db, evm_env);

        // Verify standard precompiles exist
        let identity_address = Address::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4]);
        assert!(evm.precompiles().get(&identity_address).is_some(), "Identity precompile should exist");

        // Verify privacy precompiles exist
        assert!(
            evm.precompiles().get(&REGISTRY_PRECOMPILE_ADDRESS).is_some(),
            "Registry precompile should exist at 0x200"
        );
        assert!(
            evm.precompiles().get(&AUTH_PRECOMPILE_ADDRESS).is_some(),
            "Auth precompile should exist at 0x201"
        );
    }

    #[test]
    fn test_privacy_evm_factory_with_inspector() {
        let factory = PrivacyEvmFactory::new();
        let db = EmptyDB::default();
        let evm_env = EvmEnv::new(
            CfgEnv::new_with_spec(OpSpecId::CANYON),
            Default::default(),
        );

        let evm = factory.create_evm_with_inspector(db, evm_env, NoOpInspector {});

        // Verify privacy precompiles exist
        assert!(
            evm.precompiles().get(&REGISTRY_PRECOMPILE_ADDRESS).is_some(),
            "Registry precompile should exist"
        );
        assert!(
            evm.precompiles().get(&AUTH_PRECOMPILE_ADDRESS).is_some(),
            "Auth precompile should exist"
        );
    }
}
