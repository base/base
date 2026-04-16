//! Base-specific [`OpContextTr`] trait alias and [`BaseError`] type alias.
use revm::{
    context_interface::{Cfg, ContextTr, Database, JournalTr, result::EVMError},
    state::EvmState,
};

use crate::{L1BlockInfo, OpSpecId, OpTransactionError, transaction::OpTxTr};

/// Trait alias for the context type required by [`BaseEvm`][crate::BaseEvm].
///
/// Satisfied by [`crate::OpContext`] for any database, binding the transaction type to
/// [`OpTxTr`], the spec to [`OpSpecId`], and the chain extension to [`L1BlockInfo`].
pub trait OpContextTr:
    ContextTr<
        Journal: JournalTr<State = EvmState>,
        Tx: OpTxTr,
        Cfg: Cfg<Spec = OpSpecId>,
        Chain = L1BlockInfo,
    >
{
}

impl<T> OpContextTr for T where
    T: ContextTr<
            Journal: JournalTr<State = EvmState>,
            Tx: OpTxTr,
            Cfg: Cfg<Spec = OpSpecId>,
            Chain = L1BlockInfo,
        >
{
}

/// Error type for [`BaseEvm`][crate::BaseEvm] execution, parameterized over the database
/// error type [`DB`].
pub type BaseError<DB> = EVMError<<DB as Database>::Error, OpTransactionError>;

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use revm::{
        ExecuteEvm, SystemCallEvm,
        database::{InMemoryDB, State},
    };

    use crate::{Builder, DefaultOp, OpContext};

    /// Verifies that the system call caller is loaded into the EVM state cache so it appears in the
    /// execution witness.
    ///
    /// The state cache (`State.cache.accounts`) is exactly what `ExecutionWitnessRecord` reads to
    /// build the `hashed_state` fed to `state_provider.witness(...)`. Without the
    /// `load_account_with_code_mut` call in `system_call_one_with_caller`, the caller account
    /// would not be cached and would be absent from the generated witness, breaking Geth proof
    /// compatibility.
    ///
    /// See: <https://github.com/bluealloy/revm/issues/3484>
    #[test]
    fn system_call_caller_appears_in_witness() {
        let caller = Address::repeat_byte(0xCA);
        let contract = Address::repeat_byte(0xAB);

        // Use State with bundle tracking, mirroring the witness generation path in
        // Builder::witness and debug_executionWitness.
        let state =
            State::builder().with_database(InMemoryDB::default()).with_bundle_update().build();

        let ctx = OpContext::op().with_db(state);
        let mut evm = ctx.build_op();

        // Execute a system call. This internally calls `load_account_with_code_mut(caller)`,
        // causing the State DB to load and cache the caller's account in `State.cache.accounts`.
        let _ = evm.system_call_one_with_caller(caller, contract, Default::default());

        // Finalize to flush the journal, then inspect the underlying State cache.
        // `ExecutionWitnessRecord::from_executed_state` iterates `State.cache.accounts` to build
        // the hashed state, so the caller must appear here to be included in the witness.
        let _ = evm.finalize();
        let state = evm.into_context().journaled_state.database;

        assert!(
            state.cache.accounts.contains_key(&caller),
            "system call caller must be in state cache for Geth proof compatibility"
        );
    }
}
