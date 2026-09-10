//! End-to-end precompile execution and gas accounting regressions.
//! The scripted providers return deliberately invalid gas usage to exercise runtime validation.

use base_execution_evm_runtime::{
    AccountInfo, Address, AddressSet, Bytes, CallInputs, Cfg, ContextTr,
    CryptoPrecompileOutput as PrecompileOutput, CryptoPrecompileStatus as PrecompileStatus,
    EthInstructions, EthPrecompiles, EvmMachine, ExecuteEvm, ExecutionResult, FrameStack,
    HaltReason, InMemoryDB, InstructionResult, InterpreterResult, MainContext, OutOfGasError,
    PrecompileProvider, TxEnv, TxKind, U256, address, hardfork::SpecId,
    precompile_output_to_interpreter_result,
};

/// Test-only address that hosts an over-spending precompile.
const OVERSPEND_PRECOMPILE: Address = address!("0000000000000000000000000000000000000100");

/// Custom precompile provider that drives the bug path: it returns a
/// `PrecompileOutput` with `status = Success` and `gas_used = u64::MAX` while
/// `gas_limit` is finite. Without the fix, `record_regular_cost`'s `false` return
/// is discarded so the call lands as `Return` with the gas tracker untouched —
/// the transaction succeeds and refunds the precompile's "free" gas. With the fix,
/// the helper converts the over-spend into `PrecompileOOG`, halting the tx.
#[derive(Debug)]
struct OverspendingPrecompiles {
    inner: EthPrecompiles,
    warm: AddressSet,
}

impl OverspendingPrecompiles {
    fn new(spec: SpecId) -> Self {
        let inner = EthPrecompiles::new(spec);
        let mut warm = AddressSet::default();
        warm.clone_from(inner.warm_addresses());
        warm.insert(OVERSPEND_PRECOMPILE);
        Self { inner, warm }
    }
}

impl<CTX> PrecompileProvider<CTX> for OverspendingPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = SpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        let changed = <EthPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.inner, spec);
        self.warm.clone_from(self.inner.warm_addresses());
        self.warm.insert(OVERSPEND_PRECOMPILE);
        changed
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        if inputs.bytecode_address == OVERSPEND_PRECOMPILE {
            let output = PrecompileOutput {
                status: PrecompileStatus::Success,
                gas_used: u64::MAX,
                gas_refunded: 0,
                state_gas_used: 0,
                state_gas_spilled: 0,
                reservoir: inputs.reservoir,
                bytes: Bytes::from_static(b"unreliable"),
            };
            return Ok(Some(precompile_output_to_interpreter_result(output, inputs.gas_limit)));
        }
        <EthPrecompiles as PrecompileProvider<CTX>>::run(&mut self.inner, context, inputs)
    }

    fn warm_addresses(&self) -> &AddressSet {
        &self.warm
    }
}

/// The spilled portion of a precompile's state gas must reach the frame's gas
/// tracker, otherwise a rollback credits it to the reservoir instead of regular
/// gas (EIP-8037).
#[test]
fn precompile_output_propagates_spilled_state_gas() {
    let output = PrecompileOutput {
        status: PrecompileStatus::Success,
        // 10 regular + 30 state gas, of which 20 spilled out of the 10 gas reservoir
        gas_used: 40,
        gas_refunded: 0,
        state_gas_used: 30,
        state_gas_spilled: 20,
        reservoir: 0,
        bytes: Bytes::new(),
    };
    let mut result = precompile_output_to_interpreter_result(output, 100);

    assert_eq!(result.result, InstructionResult::Return);
    assert_eq!(result.gas.state_gas_spent(), 30);
    assert_eq!(result.gas.state_gas_spilled(), 20);
    assert_eq!(result.gas.remaining(), 60);

    // rollback returns the spilled part to regular gas and the rest to the reservoir
    result.gas.rollback_state_gas();
    assert_eq!(result.gas.remaining(), 80);
    assert_eq!(result.gas.reservoir(), 10);
    assert_eq!(result.gas.state_gas_spent(), 0);
    assert_eq!(result.gas.state_gas_spilled(), 0);
}

/// A precompile that reports more gas than its limit is turned into an OOG halt
/// with all gas consumed and no output bytes.
#[test]
fn precompile_output_overspend_is_oog() {
    let output = PrecompileOutput::new(u64::MAX, Bytes::from_static(b"out"), 0);
    let result = precompile_output_to_interpreter_result(output, 100);
    assert_eq!(result.result, InstructionResult::PrecompileOOG);
    assert_eq!(result.gas.remaining(), 0);
    assert!(result.output.is_empty());
}

/// End-to-end regression test for Bug 3. A transaction targets a custom precompile
/// that lies about its gas usage. The fix turns this into an `OutOfGas(Precompile)`
/// halt; without the fix it is silently treated as a successful call.
#[test]
fn overspending_precompile_halts_tx_with_precompile_oog() {
    let caller = address!("0000000000000000000000000000000000000001");
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        caller,
        AccountInfo { balance: U256::from(10).pow(U256::from(18)), ..Default::default() },
    );

    let spec = SpecId::default();
    let ctx = base_execution_evm_runtime::ReferenceContext::mainnet().with_db(db);
    let mut evm = EvmMachine {
        ctx,
        inspector: (),
        instruction: EthInstructions::<_>::new_mainnet_with_spec(spec),
        precompiles: OverspendingPrecompiles::new(spec),
        frame_stack: FrameStack::new_prealloc(8),
    };

    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(OVERSPEND_PRECOMPILE))
        .gas_limit(100_000)
        .build()
        .unwrap();

    let exec = evm.transact_one(tx).expect("handler returned an error");

    match exec {
        ExecutionResult::Halt { reason, .. } => {
            assert_eq!(
                reason,
                HaltReason::OutOfGas(OutOfGasError::Precompile),
                "expected precompile OOG halt for over-spending precompile",
            );
        }
        ExecutionResult::Success { .. } => panic!(
            "before-fix behavior leaked: over-spending precompile reported Success \
             instead of halting with PrecompileOOG"
        ),
        ExecutionResult::Revert { .. } => panic!("expected Halt(PrecompileOOG), got Revert"),
    }
}
