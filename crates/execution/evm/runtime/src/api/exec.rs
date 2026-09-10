//! Base execution error type.
use base_execution_evm_runtime::{BaseTransactionError, Database, EVMError};

/// Error type for [`BaseEvm`][crate::BaseEvm] execution, parameterized over the database
/// error type [`DB`].
pub type BaseError<DB> = EVMError<<DB as Database>::Error, BaseTransactionError>;

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_execution_evm_runtime::{
        BaseContext, Builder, DefaultBase, ExecuteEvm, InMemoryDB, State, SystemCallEvm,
    };

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

        let ctx = BaseContext::base().with_db(state);
        let mut evm = ctx.build_base();

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
