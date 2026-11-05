//! Custom EVM inspector for metering per-contract opcode and precompile gas usage.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use alloy_primitives::{Address, map::HashMap};
use base_common_evm2::BaseEvmTypes;
use evm2::{
    Inspector,
    interpreter::{
        InstrStop, Interpreter, Message, MessageKind, MessageResult,
        opcode::{OpCode, op},
    },
};

use crate::meter::MeteredOpcodes;

/// Accumulated gas data for a single opcode executed by one contract.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OpcodeGasUsage {
    /// Number of times this opcode was executed.
    pub(crate) count: u64,
    /// Total gas consumed across all executions.
    pub(crate) gas_used: u64,
}

/// Accumulated gas data for a single precompile address.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PrecompileGasUsage {
    /// Number of calls to this precompile.
    pub(crate) count: u64,
    /// Total gas consumed across all calls.
    pub(crate) gas_used: u64,
}

/// A metered opcode waiting for its `step_end`.
#[derive(Debug, Clone, Copy)]
struct OpcodeFrame {
    contract_address: Address,
    opcode: OpCode,
    depth: u16,
    gas_remaining: u64,
    child_gas_used: u64,
}

/// EVM inspector that tracks per-contract opcode gas usage and precompile call costs.
///
/// Opcode gas is keyed by the current EVM target address (the frame message's destination), which
/// is also the address used by storage opcodes. This keeps storage-related opcode costs separated
/// by the contract whose storage context is being executed.
///
/// A call or create opcode is charged only its own overhead: the gas its child frame spent is
/// excluded, and the child frame's opcodes are metered on their own.
///
/// When no opcodes are configured, `step`/`step_end` are no-ops to avoid per-opcode overhead when
/// only precompile tracking is needed.
#[derive(Debug)]
pub(crate) struct MeteringInspector {
    opcode_gas: HashMap<(Address, OpCode), OpcodeGasUsage>,
    precompile_gas: HashMap<Address, PrecompileGasUsage>,
    metered_opcodes: Arc<MeteredOpcodes>,
    /// Metered opcodes awaiting `step_end`, innermost frame last. A call or create opcode stays
    /// pending while its child frame runs.
    pending: Vec<OpcodeFrame>,
}

impl MeteringInspector {
    /// Creates a new inspector that tracks the configured precompiles and opcodes.
    pub(crate) fn new(metered_opcodes: Arc<MeteredOpcodes>) -> Self {
        Self {
            opcode_gas: HashMap::default(),
            precompile_gas: HashMap::default(),
            metered_opcodes,
            pending: Vec::new(),
        }
    }

    /// Extracts the accumulated opcode gas data and resets the map.
    ///
    /// Call this after each transaction to get per-transaction opcode data.
    pub(crate) fn take_opcode_gas(&mut self) -> HashMap<(Address, OpCode), OpcodeGasUsage> {
        self.pending.clear();
        std::mem::take(&mut self.opcode_gas)
    }

    /// Extracts the accumulated precompile gas data and resets the map.
    ///
    /// Call this after each transaction to get per-transaction precompile data.
    pub(crate) fn take_precompile_gas(&mut self) -> HashMap<Address, PrecompileGasUsage> {
        std::mem::take(&mut self.precompile_gas)
    }

    /// Excludes a child frame's spent gas from the pending call or create opcode that started it.
    fn exclude_child_gas(&mut self, depth: u16, opcode: u8, child_gas_used: u64) {
        if let Some(frame) = self.pending.last_mut()
            && frame.depth == depth
            && frame.opcode == opcode
        {
            frame.child_gas_used = frame.child_gas_used.saturating_add(child_gas_used);
        }
    }
}

impl Inspector<BaseEvmTypes> for MeteringInspector {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        if self.metered_opcodes.opcodes.is_empty() {
            return;
        }

        let Some(opcode) = OpCode::new(interp.opcode()) else { return };
        if !self.metered_opcodes.opcodes.contains(&opcode) {
            return;
        }

        let message = interp.message();
        let contract_address = message.destination;
        let entry = self.opcode_gas.entry((contract_address, opcode)).or_default();
        entry.count = entry.count.saturating_add(1);
        self.pending.push(OpcodeFrame {
            contract_address,
            opcode,
            depth: message.depth,
            gas_remaining: interp.gas().remaining(),
            child_gas_used: 0,
        });
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        if self.pending.last().is_none_or(|frame| frame.depth != interp.message().depth) {
            return;
        }
        let Some(frame) = self.pending.pop() else { return };
        // A charge that fails leaves the frame without gas; the opcode itself consumed nothing.
        let gas_cost = if interp.result().is_err_and(InstrStop::is_out_of_gas) {
            0
        } else {
            frame
                .gas_remaining
                .saturating_sub(interp.gas().remaining())
                .saturating_sub(frame.child_gas_used)
        };
        let entry = self.opcode_gas.entry((frame.contract_address, frame.opcode)).or_default();
        entry.gas_used = entry.gas_used.saturating_add(gas_cost);
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        result: &mut MessageResult<BaseEvmTypes>,
    ) {
        // The top-level call has no parent opcode to exclude its gas from.
        if message.depth != interp.version().initial_call_depth {
            let opcode = match message.kind {
                MessageKind::Call => op::CALL,
                MessageKind::CallCode => op::CALLCODE,
                MessageKind::DelegateCall => op::DELEGATECALL,
                MessageKind::StaticCall => op::STATICCALL,
                _ => return,
            };
            self.exclude_child_gas(interp.message().depth, opcode, result.gas.spent());
        }

        if !self.metered_opcodes.meters_any_precompile() {
            return;
        }

        let target = message.call_target;
        if self.metered_opcodes.meters_precompile(target) {
            let entry = self.precompile_gas.entry(target).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.gas_used = entry.gas_used.saturating_add(result.gas.spent());
        }
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        result: &mut MessageResult<BaseEvmTypes>,
    ) {
        if message.depth == interp.version().initial_call_depth {
            return;
        }

        let opcode = match message.kind {
            MessageKind::Create => op::CREATE,
            MessageKind::Create2 => op::CREATE2,
            _ => return,
        };
        self.exclude_child_gas(interp.message().depth, opcode, result.gas.spent());
    }
}

/// Installs a shared [`MeteringInspector`] on an EVM while the caller keeps reading what it
/// recorded between transactions.
#[derive(Debug, Clone)]
pub(crate) struct SharedMeteringInspector(pub(crate) Rc<RefCell<MeteringInspector>>);

impl Inspector<BaseEvmTypes> for SharedMeteringInspector {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        self.0.borrow_mut().step(interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        self.0.borrow_mut().step_end(interp);
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        result: &mut MessageResult<BaseEvmTypes>,
    ) {
        self.0.borrow_mut().call_end(interp, message, result);
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        result: &mut MessageResult<BaseEvmTypes>,
    ) {
        self.0.borrow_mut().create_end(interp, message, result);
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, Signed, TxLegacy, transaction::Recovered};
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use base_common_consensus::BaseTxEnvelope;
    use base_common_evm2::BaseTxEnvelope as BaseTxEnv;
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_execution_evm::BaseEvmConfig;
    use evm2::{
        bytecode::Bytecode,
        evm::{AccountInfo, InMemoryDB},
    };
    use reth_evm::{ConfigureEvm, FromRecoveredTx};

    use super::*;

    const SENDER: Address = Address::repeat_byte(0x11);
    const CONTRACT: Address = Address::repeat_byte(0x22);
    const EMPTY_CALLEE: Address = Address::repeat_byte(0x33);

    /// `SSTORE(0, 1)` then a value-less `CALL` to [`EMPTY_CALLEE`].
    fn contract_code() -> Bytes {
        let mut code = vec![0x60, 0x01, 0x60, 0x00, 0x55];
        code.extend_from_slice(&[0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00]);
        code.push(0x73);
        code.extend_from_slice(EMPTY_CALLEE.as_slice());
        code.extend_from_slice(&[0x61, 0xff, 0xff, 0xf1, 0x50, 0x00]);
        Bytes::from(code)
    }

    fn meter(opcodes: &[&str]) -> HashMap<(Address, OpCode), OpcodeGasUsage> {
        let names: Vec<_> = opcodes.iter().map(ToString::to_string).collect();
        let metered = Arc::new(MeteredOpcodes::parse(&names).unwrap());
        let inspector = Rc::new(RefCell::new(MeteringInspector::new(metered)));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &SENDER,
            AccountInfo::default().with_balance(U256::from(1_000_000_000_000_000_000u128)),
        );
        db.insert_account_info(
            &CONTRACT,
            AccountInfo::default().with_nonce(1).with_code(Bytecode::new_raw(contract_code())),
        );
        let evm_config =
            BaseEvmConfig::base(Arc::new(BaseChainSpecBuilder::base_mainnet().build()));
        let header = Header { number: 1, gas_limit: 30_000_000, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).unwrap();
        let mut evm = evm_config.evm_with_env(db, evm_env);
        evm.set_inspector(SharedMeteringInspector(Rc::clone(&inspector)));

        let tx = TxLegacy { gas_limit: 200_000, to: TxKind::Call(CONTRACT), ..Default::default() };
        let tx = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            Default::default(),
        ));
        let tx = Recovered::new_unchecked(tx, SENDER);
        let tx = Recovered::new_unchecked(BaseTxEnv::from_recovered_tx(tx), SENDER);
        let result = evm.transact(&tx).unwrap().discard();
        assert!(result.is_success());

        inspector.borrow_mut().take_opcode_gas()
    }

    #[test]
    fn opcodes_are_charged_their_own_cost_by_executing_contract() {
        let opcode_gas = meter(&["SSTORE", "CALL"]);

        let sstore = opcode_gas[&(CONTRACT, OpCode::new(op::SSTORE).unwrap())];
        assert_eq!((sstore.count, sstore.gas_used), (1, 22_100), "cold zero-to-nonzero SSTORE");
        // A call is charged its cold account access, not the gas it forwards to its callee.
        let call = opcode_gas[&(CONTRACT, OpCode::new(op::CALL).unwrap())];
        assert_eq!((call.count, call.gas_used), (1, 2_600), "cold value-less CALL");
    }

    #[test]
    fn unmetered_opcodes_are_not_recorded() {
        let opcode_gas = meter(&["SSTORE"]);

        assert_eq!(opcode_gas.len(), 1);
    }
}
