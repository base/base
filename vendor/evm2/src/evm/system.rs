//! System transaction execution.
//!
//! System calls execute bytecode already present at the target system contract address. The EVM
//! does not install or inject protocol system contract bytecode; callers that use these hooks are
//! responsible for making the target account and code available in the backing database or overlay
//! before calling [`Evm::system_call`]. Calling an address without code succeeds as an empty call
//! and produces no state changes.

use alloc::vec::Vec;
#[cfg(feature = "async")]
use core::future::Future;

use alloy_primitives::{Address, Bytes, TxKind, U256, address};

use super::{Evm, ExecutedTx, TxResult, TxResultExt};
#[cfg(feature = "async")]
use super::{SendEvmRef, r#async};
use crate::{
    EvmTypes,
    env::TxEnvExt,
    ethereum::{execute_initial_frame, prepare_initial_frame},
    interpreter::GasTracker,
    registry::{HandlerError, HandlerResult},
    version::{EvmFeatures, GasId},
};

/// Caller address used by execution-layer system calls.
pub const SYSTEM_ADDRESS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

/// Gas limit used by execution-layer system calls.
pub const SYSTEM_CALL_GAS_LIMIT: u64 = 30_000_000;

/// Upper bound on the number of new storage slots a single system call is expected to write.
///
/// EIP-8037 (Amsterdam) sizes the system-call state-gas reservoir as this many `SSTORE` new-slot
/// state charges; state gas beyond the reservoir spills into the execution gas budget.
pub const SYSTEM_MAX_SSTORES_PER_CALL: u64 = 16;

/// EIP-4788 beacon roots system contract address.
pub const BEACON_ROOTS_ADDRESS: Address = address!("0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02");

/// EIP-2935 historical block hashes system contract address.
pub const HISTORY_STORAGE_ADDRESS: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");

/// EIP-7002 withdrawal request system contract address.
pub const WITHDRAWAL_REQUEST_ADDRESS: Address =
    address!("0x00000961Ef480Eb55e80D19ad83579A64c007002");

/// EIP-7251 consolidation request system contract address.
pub const CONSOLIDATION_REQUEST_ADDRESS: Address =
    address!("0x0000BBdDc7CE488642fb579F8B00f3a590007251");

/// EIP-8282 builder deposit request system contract address (request type `0x03`).
pub const BUILDER_DEPOSIT_REQUEST_ADDRESS: Address =
    address!("0x0000BFF46984E3725691FA540A8C7589300D8282");

/// EIP-8282 builder exit request system contract address (request type `0x04`).
pub const BUILDER_EXIT_REQUEST_ADDRESS: Address =
    address!("0x000064D678505AD48F8CCB093BC65613800E8282");

/// System transaction input for [`Evm::system_call`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemTx {
    /// Caller address.
    pub caller: Address,
    /// Target system contract address.
    pub system_contract_address: Address,
    /// Calldata.
    pub data: Bytes,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl Default for SystemTx {
    #[inline]
    fn default() -> Self {
        Self {
            caller: SYSTEM_ADDRESS,
            system_contract_address: Address::ZERO,
            data: Bytes::new(),
            _non_exhaustive: (),
        }
    }
}

impl SystemTx {
    /// Creates a new system transaction from [`SYSTEM_ADDRESS`].
    #[inline]
    pub fn new(system_contract_address: Address, data: Bytes) -> Self {
        Self { system_contract_address, data, ..Default::default() }
    }

    /// Sets the caller address.
    #[inline]
    pub const fn with_caller(mut self, caller: Address) -> Self {
        self.caller = caller;
        self
    }
}

impl<'a, T: EvmTypes> Evm<'a, T> {
    /// Executes a system call.
    ///
    /// System calls bypass normal transaction validation, nonce updates, fee charging, gas refunds,
    /// and beneficiary rewards. They execute a top-level `CALL` with zero value and
    /// [`SYSTEM_CALL_GAS_LIMIT`] gas, then finalize and return an executed transaction handle.
    ///
    /// The target system contract bytecode must already be present in state. This method does not
    /// deploy protocol system contracts or synthesize their bytecode.
    pub fn system_call(&mut self, tx: SystemTx) -> HandlerResult<ExecutedTx<'_, 'a, T>> {
        self.clear_top_level_error_state();
        // System calls are not inspected.
        let inspector = self.inspector.take();
        let result = self.execute_system_call(tx);
        self.inspector = inspector;
        match result {
            Ok(outcome) => Ok(self.finish_executed_tx(outcome)),
            Err(error) => {
                self.state.clear_transaction_state();
                Err(error)
            }
        }
    }

    /// Executes a system call without finalizing transaction state.
    ///
    /// This is the handler-level counterpart to [`Self::system_call`]. It allows a transaction
    /// registry handler to execute system-call semantics and return the outcome to the outer
    /// transaction lifecycle for finalization.
    pub fn execute_system_call(&mut self, tx: SystemTx) -> HandlerResult<TxResult<T>> {
        let SystemTx { caller, system_contract_address, data, .. } = tx;
        self.state.prewarm(&system_contract_address);
        let tx_env = TxEnvExt {
            origin: caller,
            gas_price: U256::ZERO,
            chain_id: U256::from(self.version().chain_id),
            blob_hashes: Vec::new(),
            ext: T::TxEnvExt::default(),
            _non_exhaustive: (),
        };
        // EIP-8037: system calls start with a state-gas reservoir sized for
        // `SYSTEM_MAX_SSTORES_PER_CALL` new-slot writes (execution-specs
        // `process_unchecked_system_transaction`).
        let reservoir = if self.feature(EvmFeatures::EIP8037) {
            self.version().gas_params.get(GasId::SstoreSetState) as u64
                * SYSTEM_MAX_SSTORES_PER_CALL
        } else {
            0
        };
        let mut tx_gas =
            GasTracker::new_with_execution_gas_and_reservoir(SYSTEM_CALL_GAS_LIMIT, reservoir);
        let frame = prepare_initial_frame(
            self,
            caller,
            0,
            TxKind::Call(system_contract_address),
            &data,
            U256::ZERO,
            &mut tx_gas,
        )?;
        let result = execute_initial_frame(
            self,
            &tx_env,
            frame,
            &mut tx_gas,
            SYSTEM_CALL_GAS_LIMIT,
            reservoir,
        );
        if let Some(code) = self.error_code {
            return Err(HandlerError::Fatal(code));
        }
        let gas_spent = SYSTEM_CALL_GAS_LIMIT.saturating_sub(result.gas.remaining());
        let gas_refunded = if result.stop.is_success() && result.gas.refunded() > 0 {
            result.gas.refunded() as u64
        } else {
            0
        };
        Ok(TxResultExt {
            status: result.stop.is_success(),
            total_gas_spent: gas_spent,
            state_gas_spent: result.gas.state_gas_spent().max(0) as u64,
            refunded: gas_refunded,
            floor_gas: 0,
            stop: result.stop,
            output: result.output,
            ..TxResultExt::default()
        })
    }

    /// Executes a system call on an async fiber.
    ///
    /// This must be used with an async database adapter such as
    /// [`evm::async::AsyncDb`](crate::evm::async::AsyncDb) to take
    /// advantage of yielding database I/O. With a synchronous database this is mostly equivalent to
    /// running the synchronous system call on a fiber.
    ///
    /// This returns a local future and does not require the erased database, precompile provider,
    /// or optional inspector to be `Send`. Use [`Evm::system_call_async_send`] when the returned
    /// future must be `Send`.
    #[cfg(feature = "async")]
    #[inline]
    pub fn system_call_async(
        &mut self,
        tx: SystemTx,
    ) -> impl Future<Output = r#async::AsyncResult<ExecutedTx<'_, 'a, T>, HandlerError>> + '_ {
        let stack = self.async_stack();
        // SAFETY: The returned future owns the exclusive `&mut self` borrow, so nothing else can
        // access the EVM stack slot until that future is dropped.
        unsafe { r#async::on_local_fiber_result_with_stack(stack, move || self.system_call(tx)) }
    }

    /// Executes a system call on an async fiber and returns a `Send` future.
    ///
    /// Before calling it, the current erased database, precompile provider, and optional inspector
    /// must be verified with [`Evm::evm_is_send`] or [`Evm::evm_is_send_with_inspector`].
    #[cfg(feature = "async")]
    #[inline]
    pub fn system_call_async_send(
        &mut self,
        tx: SystemTx,
    ) -> impl Future<Output = r#async::AsyncResult<ExecutedTx<'_, 'a, T>, HandlerError>> + Send + '_
    {
        self.assert_erased_send();
        let stack = self.async_stack();
        let evm = SendEvmRef { evm: self };
        // SAFETY: The returned future owns the exclusive `&mut self` borrow, so nothing else can
        // access the EVM stack slot until that future is dropped. The send marker checked above
        // requires all erased EVM fields to have been verified by `Evm::evm_is_send`.
        unsafe {
            r#async::on_fiber_result_with_stack(stack, move || {
                let SendEvmRef { evm } = evm;
                evm.system_call(tx)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BaseEvmTypes, ErrorCode, Precompiles, SpecId,
        bytecode::Bytecode,
        env::BlockEnvExt,
        evm::{AccountInfo, InMemoryDB},
        interpreter::{GasTracker, InstrStop, Message, op},
        precompiles::{Precompile, PrecompileError, PrecompileId, PrecompileResult},
        registry::{HandlerError, TxRegistry},
    };

    type TestEvm = Evm<'static, BaseEvmTypes>;

    #[test]
    fn system_call_uses_system_sender_without_fee_accounting() {
        let contract = Address::from([0x42; 20]);
        let beneficiary = Address::from([0x99; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[
            op::CALLER,
            op::PUSH1,
            0,
            op::SSTORE,
            op::ORIGIN,
            op::PUSH1,
            1,
            op::SSTORE,
            op::STOP,
        ]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let block = BlockEnvExt { beneficiary, basefee: U256::from(7), ..BlockEnvExt::default() };
        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            block,
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::OSAKA),
        );

        let result = evm.system_call(SystemTx::new(contract, Bytes::new())).unwrap().detach();

        assert!(result.result.status);
        assert!(result.result.tx_gas_used() < SYSTEM_CALL_GAS_LIMIT);
        let unchanged = |address| {
            result.pending_state.accounts.get(address).is_none_or(|entry| !entry.is_changed())
        };
        assert!(unchanged(&SYSTEM_ADDRESS));
        assert!(unchanged(&beneficiary));
        let storage = &result.pending_state.storage.get(&contract).expect("storage changed").slots;
        let system_address = U256::from_be_slice(SYSTEM_ADDRESS.as_slice());
        assert_eq!(storage.get(&U256::ZERO).map(|slot| slot.value.current), Some(system_address));
        assert_eq!(storage.get(&U256::ONE).map(|slot| slot.value.current), Some(system_address));
    }

    #[test]
    fn system_call_uses_custom_caller() {
        let caller = Address::from([0x11; 20]);
        let contract = Address::from([0x42; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[
            op::CALLER,
            op::PUSH1,
            0,
            op::SSTORE,
            op::ORIGIN,
            op::PUSH1,
            1,
            op::SSTORE,
            op::STOP,
        ]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::OSAKA),
        );

        let result = evm
            .system_call(SystemTx::new(contract, Bytes::new()).with_caller(caller))
            .unwrap()
            .detach();

        assert!(result.result.status);
        let storage = &result.pending_state.storage.get(&contract).expect("storage changed").slots;
        let caller = U256::from_be_slice(caller.as_slice());
        assert_eq!(storage.get(&U256::ZERO).map(|slot| slot.value.current), Some(caller));
        assert_eq!(storage.get(&U256::ONE).map(|slot| slot.value.current), Some(caller));
    }

    #[test]
    fn system_call_starts_with_warm_system_contract() {
        let contract = Address::from([0x42; 20]);
        let code =
            Bytecode::new_legacy(Bytes::from_static(&[op::ADDRESS, op::EXTCODESIZE, op::STOP]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::OSAKA),
        );

        let outcome = evm.system_call(SystemTx::new(contract, Bytes::new())).unwrap().discard();

        assert!(outcome.status);
        assert!(
            outcome.tx_gas_used() < 1_000,
            "system contract should be warm before execution, got {} gas used",
            outcome.tx_gas_used()
        );
    }

    #[test]
    fn system_call_to_missing_code_is_noop() {
        let contract = Address::from([0x42; 20]);
        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            Precompiles::base(SpecId::OSAKA),
        );

        let result = evm.system_call(SystemTx::new(contract, Bytes::new())).unwrap().detach();

        assert!(result.result.status);
        assert_eq!(result.result.tx_gas_used(), 0);
        assert!(!result.pending_state.is_changed());
    }

    #[test]
    fn system_call_reverts_state_changes() {
        let contract = Address::from([0x42; 20]);
        let code = Bytecode::new_legacy(Bytes::from_static(&[
            op::PUSH1,
            1,
            op::PUSH1,
            0,
            op::SSTORE,
            op::PUSH1,
            0,
            op::PUSH1,
            0,
            op::REVERT,
        ]));
        let mut database = InMemoryDB::default();
        database.insert_account_info(&contract, AccountInfo::default().with_code(code));
        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            database,
            Precompiles::base(SpecId::OSAKA),
        );

        let result = evm.system_call(SystemTx::new(contract, Bytes::new())).unwrap().detach();

        assert!(!result.result.status);
        assert_eq!(result.result.stop, InstrStop::Revert);
        assert!(!result.pending_state.is_changed());
    }

    #[test]
    fn system_call_reports_fatal_precompile_error() {
        const FATAL_PRECOMPILE_ADDRESS: Address = Address::with_last_byte(0x43);

        #[derive(Debug)]
        struct TestPrecompileError;

        impl core::fmt::Display for TestPrecompileError {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("test precompile error")
            }
        }

        impl core::error::Error for TestPrecompileError {}

        fn fatal_precompile(
            _evm: &mut Evm<'_, BaseEvmTypes>,
            _message: &Message,
            _gas: &mut GasTracker,
        ) -> PrecompileResult {
            Err(PrecompileError::fatal(TestPrecompileError))
        }

        let mut precompiles = Precompiles::base(SpecId::OSAKA);
        precompiles.as_map_mut().insert(Precompile::new(
            FATAL_PRECOMPILE_ADDRESS,
            PrecompileId::custom("fatal-test"),
            fatal_precompile,
        ));

        let mut evm = TestEvm::new(
            SpecId::OSAKA,
            BlockEnvExt::default(),
            TxRegistry::new(),
            InMemoryDB::default(),
            precompiles,
        );

        let result = evm.system_call(SystemTx::new(FATAL_PRECOMPILE_ADDRESS, Bytes::new()));

        assert_eq!(
            result.map(ExecutedTx::discard),
            Err(HandlerError::Fatal(ErrorCode::FATAL_PRECOMPILE))
        );
        assert_eq!(evm.error_code(), Some(ErrorCode::FATAL_PRECOMPILE));
    }
}
