use core::ops::{Deref, DerefMut};

use alloy_evm::{Database, Evm, EvmEnv};
use alloy_primitives::{Address, Bytes};
use op_revm::{
    OpContext, OpHaltReason, OpSpecId, OpTransaction, OpTransactionError,
    precompiles::OpPrecompiles,
};
use revm::{
    ExecuteEvm, InspectEvm, Inspector, SystemCallEvm,
    context::{BlockEnv, TxEnv},
    context_interface::result::{EVMError, ResultAndState},
    handler::{PrecompileProvider, instructions::EthInstructions},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
};

/// OP EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// [`OpEvm`](op_revm::OpEvm) type.
#[allow(missing_debug_implementations)] // missing revm::OpContext Debug impl
pub struct OpEvm<DB: Database, I, P = OpPrecompiles> {
    pub(crate) inner:
        op_revm::OpEvm<OpContext<DB>, I, EthInstructions<EthInterpreter, OpContext<DB>>, P>,
    pub(crate) inspect: bool,
}

impl<DB: Database, I, P> OpEvm<DB, I, P> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &OpContext<DB> {
        &self.inner.0.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub const fn ctx_mut(&mut self) -> &mut OpContext<DB> {
        &mut self.inner.0.ctx
    }
}

impl<DB: Database, I, P> OpEvm<DB, I, P> {
    /// Creates a new OP EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`OpEvm`](op_revm::OpEvm) should be invoked on [`Evm::transact`].
    pub const fn new(
        evm: op_revm::OpEvm<OpContext<DB>, I, EthInstructions<EthInterpreter, OpContext<DB>>, P>,
        inspect: bool,
    ) -> Self {
        Self { inner: evm, inspect }
    }
}

impl<DB: Database, I, P> Deref for OpEvm<DB, I, P> {
    type Target = OpContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for OpEvm<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, P> Evm for OpEvm<DB, I, P>
where
    DB: Database,
    I: Inspector<OpContext<DB>>,
    P: PrecompileProvider<OpContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = OpTransaction<TxEnv>;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = P;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let revm::Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.0.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.0.ctx.journaled_state.database,
            &self.inner.0.inspector,
            &self.inner.0.precompiles,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.0.ctx.journaled_state.database,
            &mut self.inner.0.inspector,
            &mut self.inner.0.precompiles,
        )
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec};

    use alloy_evm::{
        EvmFactory, EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::{Address, U256};
    use op_revm::precompiles::{bls12_381, bn254_pair};
    use revm::{context::CfgEnv, database::EmptyDB, precompile::PrecompileError};

    use super::*;
    use crate::OpEvmFactory;

    #[test]
    fn test_precompiles_jovian_fail() {
        let mut evm = OpEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(CfgEnv::new_with_spec(OpSpecId::JOVIAN), BlockEnv::default()),
        );

        let (precompiles, ctx) = (&mut evm.inner.0.precompiles, &mut evm.inner.0.ctx);

        let jovian_precompile = precompiles.get(bn254_pair::JOVIAN.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bn254_pair::JOVIAN_MAX_INPUT_SIZE + 1],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PrecompileError::Bn254PairLength));

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_G1_MSM.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE + 1],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("G1MSM input length too long"));

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_G2_MSM.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE + 1],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("G2MSM input length too long"));

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_PAIRING.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE + 1],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Pairing input length too long"));
    }

    #[test]
    fn test_precompiles_jovian() {
        let mut evm = OpEvmFactory::default().create_evm(
            EmptyDB::default(),
            EvmEnv::new(CfgEnv::new_with_spec(OpSpecId::JOVIAN), BlockEnv::default()),
        );
        let (precompiles, ctx) = (&mut evm.inner.0.precompiles, &mut evm.inner.0.ctx);
        let jovian_precompile = precompiles.get(bn254_pair::JOVIAN.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bn254_pair::JOVIAN_MAX_INPUT_SIZE],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_ok());

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_G1_MSM.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_ok());

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_G2_MSM.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_ok());

        let jovian_precompile = precompiles.get(bls12_381::JOVIAN_PAIRING.address()).unwrap();
        let result = jovian_precompile.call(PrecompileInput {
            data: &vec![0; bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE],
            gas: u64::MAX,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            target_address: Address::ZERO,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        });

        assert!(result.is_ok());
    }
}
