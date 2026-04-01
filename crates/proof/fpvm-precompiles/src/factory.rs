//! Factory for creating EVM instances with FPVM-accelerated precompiles enabled.

use alloy_evm::{Database, EvmEnv, EvmFactory};
use base_evm::{
    DefaultOp, OpBuilder, OpContext, OpEvm, OpHaltReason, OpSpecId, OpTransaction,
    OpTransactionError,
};
use base_proof_preimage::{HintWriterClient, PreimageOracleClient};
use revm::{
    Context, Inspector,
    context::{BlockEnv, TxEnv},
    context_interface::result::EVMError,
    inspector::NoOpInspector,
};

use crate::OpFpvmPrecompiles;

/// Factory for creating EVM instances with FPVM-accelerated precompile overrides enabled.
#[derive(Debug, Clone)]
pub struct FpvmOpEvmFactory<H, O> {
    /// The hint writer.
    hint_writer: H,
    /// The oracle reader.
    oracle_reader: O,
}

impl<H, O> FpvmOpEvmFactory<H, O>
where
    H: HintWriterClient + Clone + Send + Sync + 'static,
    O: PreimageOracleClient + Clone + Send + Sync + 'static,
{
    /// Creates a new [`FpvmOpEvmFactory`].
    pub const fn new(hint_writer: H, oracle_reader: O) -> Self {
        Self { hint_writer, oracle_reader }
    }

    /// Returns a reference to the inner [`HintWriterClient`].
    pub const fn hint_writer(&self) -> &H {
        &self.hint_writer
    }

    /// Returns a reference to the inner [`PreimageOracleClient`].
    pub const fn oracle_reader(&self) -> &O {
        &self.oracle_reader
    }

    /// Returns a new [`OpFpvmPrecompiles`] instance for the given spec.
    pub fn create_precompiles(&self, spec: OpSpecId) -> OpFpvmPrecompiles<H, O> {
        OpFpvmPrecompiles::new_with_spec(spec, self.hint_writer.clone(), self.oracle_reader.clone())
    }
}

impl<H, O> EvmFactory for FpvmOpEvmFactory<H, O>
where
    H: HintWriterClient + Clone + Send + Sync + 'static,
    O: PreimageOracleClient + Clone + Send + Sync + 'static,
{
    type Evm<DB: Database, I: Inspector<OpContext<DB>>> = OpEvm<DB, I, OpFpvmPrecompiles<H, O>>;
    type Context<DB: Database> = OpContext<DB>;
    type Tx = OpTransaction<TxEnv>;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, OpTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = OpFpvmPrecompiles<H, O>;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        OpEvm::new(
            Context::op()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .build_op_with_inspector(NoOpInspector {})
                .with_precompiles(OpFpvmPrecompiles::new_with_spec(
                    spec_id,
                    self.hint_writer.clone(),
                    self.oracle_reader.clone(),
                )),
            false,
        )
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<OpSpecId>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        OpEvm::new(
            Context::op()
                .with_db(db)
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .build_op_with_inspector(inspector)
                .with_precompiles(OpFpvmPrecompiles::new_with_spec(
                    spec_id,
                    self.hint_writer.clone(),
                    self.oracle_reader.clone(),
                )),
            true,
        )
    }
}
