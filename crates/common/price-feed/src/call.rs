//! Read-only contract calls against local state.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::{
    Context, Database, MainBuilder, MainContext, SystemCallEvm,
    context::result::{EVMError, ExecutionResult},
};

/// Error making a [`StateCall`].
#[derive(Debug, thiserror::Error)]
pub enum StateCallError<E> {
    /// The EVM could not execute the call, usually because state is unreadable.
    #[error("call to {to} failed to execute: {error}")]
    Evm {
        /// Contract called.
        to: Address,
        /// Execution error.
        error: EVMError<E>,
    },
    /// The call reverted.
    #[error("call to {to} reverted")]
    Reverted {
        /// Contract called.
        to: Address,
        /// Revert data.
        output: Bytes,
    },
    /// The call halted.
    #[error("call to {to} halted")]
    Halted {
        /// Contract called.
        to: Address,
    },
    /// The call returned data that does not decode as the expected type.
    #[error("failed to decode call output from {to}: {source}")]
    Decode {
        /// Contract that returned the data.
        to: Address,
        /// Decoding error.
        source: alloy_sol_types::Error,
    },
}

/// Executes view calls against a [`revm::Database`] without committing state.
///
/// Calls run from the zero address as both caller and origin, as an
/// `eth_call` without `from` does, and skip fee and nonce checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateCall;

impl StateCall {
    /// Calls `to` with `call` against `db` and decodes the return data.
    pub fn call<DB: Database, C: SolCall>(
        db: &mut DB,
        to: Address,
        call: &C,
    ) -> Result<C::Return, StateCallError<DB::Error>> {
        let mut evm = Context::mainnet().with_db(db).build_mainnet();
        let result = evm
            .system_call_one_with_caller(Address::ZERO, to, call.abi_encode().into())
            .map_err(|error| StateCallError::Evm { to, error })?;
        match result {
            ExecutionResult::Success { output, .. } => C::abi_decode_returns(output.data())
                .map_err(|source| StateCallError::Decode { to, source }),
            ExecutionResult::Revert { output, .. } => Err(StateCallError::Reverted { to, output }),
            ExecutionResult::Halt { .. } => Err(StateCallError::Halted { to }),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use alloy_sol_types::sol;
    use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

    use super::*;
    use crate::test_utils::ViewContract;

    sol! {
        interface IProbe {
            function value() external view returns (uint256);
            function other() external view returns (uint256);
        }
    }

    const PROBE: Address = Address::repeat_byte(0x42);

    fn db(contract: ViewContract) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            PROBE,
            AccountInfo::default().with_code(Bytecode::new_raw(contract.bytecode())),
        );
        db
    }

    #[test]
    fn decodes_the_return_data_of_the_called_selector() {
        let mut db = db(ViewContract::new()
            .returns(IProbe::valueCall::SELECTOR, U256::from(7).to_be_bytes::<32>())
            .returns(IProbe::otherCall::SELECTOR, U256::from(9).to_be_bytes::<32>()));

        assert_eq!(StateCall::call(&mut db, PROBE, &IProbe::valueCall {}).unwrap(), U256::from(7));
        assert_eq!(StateCall::call(&mut db, PROBE, &IProbe::otherCall {}).unwrap(), U256::from(9));
    }

    #[test]
    fn reports_reverts_and_undecodable_output() {
        let mut db = db(ViewContract::new()
            .reverts(IProbe::valueCall::SELECTOR, [0xde, 0xad])
            .returns(IProbe::otherCall::SELECTOR, [1]));

        assert!(matches!(
            StateCall::call(&mut db, PROBE, &IProbe::valueCall {}),
            Err(StateCallError::Reverted { output, .. }) if output.as_ref() == [0xde, 0xad]
        ));
        assert!(matches!(
            StateCall::call(&mut db, PROBE, &IProbe::otherCall {}),
            Err(StateCallError::Decode { .. })
        ));
    }
}
