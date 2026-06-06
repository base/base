//! Custom EVM inspector for metering per-contract opcode and precompile gas usage.

use alloy_primitives::{
    Address,
    map::{HashMap, HashSet},
};
use revm::{
    Inspector,
    context::ContextTr,
    interpreter::{
        CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, CreateScheme,
        Interpreter,
        interpreter_types::{InputsTr, Jumps},
    },
};
use revm_bytecode::opcode::{self, OpCode};

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

/// Pending opcode frame used to attribute gas after `step_end`.
#[derive(Debug, Clone, Copy)]
struct OpcodeFrame {
    contract_address: Address,
    opcode: OpCode,
    gas_remaining: u64,
}

/// EVM inspector that tracks per-contract opcode gas usage and precompile call costs.
///
/// Opcode gas is keyed by the current EVM target address (`interp.input.target_address()`), which
/// is also the address used by storage opcodes. This keeps storage-related opcode costs separated
/// by the contract whose storage context is being executed.
///
/// When `metered_opcodes` is empty, `step`/`step_end` are no-ops to avoid
/// per-opcode overhead when only precompile tracking is needed.
#[derive(Debug)]
pub(crate) struct MeteringInspector {
    opcode_gas: HashMap<(Address, OpCode), OpcodeGasUsage>,
    precompile_gas: HashMap<Address, PrecompileGasUsage>,
    metered_precompiles: HashSet<Address>,
    metered_opcodes: HashSet<OpCode>,
    opcode_frame: Option<OpcodeFrame>,
}

impl MeteringInspector {
    /// Creates a new inspector that tracks the given precompile addresses and opcodes.
    pub(crate) fn new(
        metered_precompiles: HashSet<Address>,
        metered_opcodes: HashSet<OpCode>,
    ) -> Self {
        Self {
            opcode_gas: HashMap::default(),
            precompile_gas: HashMap::default(),
            metered_precompiles,
            metered_opcodes,
            opcode_frame: None,
        }
    }

    /// Extracts the accumulated opcode gas data and resets the map.
    ///
    /// Call this after each transaction to get per-transaction opcode data.
    pub(crate) fn take_opcode_gas(&mut self) -> HashMap<(Address, OpCode), OpcodeGasUsage> {
        self.opcode_frame = None;
        std::mem::take(&mut self.opcode_gas)
    }

    /// Extracts the accumulated precompile gas data and resets the map.
    ///
    /// Call this after each transaction to get per-transaction precompile data.
    pub(crate) fn take_precompile_gas(&mut self) -> HashMap<Address, PrecompileGasUsage> {
        std::mem::take(&mut self.precompile_gas)
    }

    /// Subtracts forwarded call/create gas so the parent opcode is charged only its own overhead.
    ///
    /// The parent opcode's `step_end` runs before revm refunds unused child-frame gas, so it sees
    /// overhead plus the full forwarded frame gas limit. By `call_end`/`create_end`, the parent
    /// opcode frame has already been popped, so subtract from the accumulated opcode entry.
    fn subtract_forwarded_gas(
        &mut self,
        contract_address: Address,
        opcode_value: u8,
        gas_limit: u64,
    ) {
        let Some(opcode) = OpCode::new(opcode_value) else { return };
        if !self.metered_opcodes.contains(&opcode) {
            return;
        }

        let entry = self.opcode_gas.entry((contract_address, opcode)).or_default();
        entry.gas_used = entry.gas_used.saturating_sub(gas_limit);
    }
}

impl<CTX> Inspector<CTX> for MeteringInspector
where
    CTX: ContextTr,
{
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = context;

        if self.metered_opcodes.is_empty() {
            return;
        }

        let Some(opcode) = OpCode::new(interp.bytecode.opcode()) else { return };
        let contract_address = interp.input.target_address();
        if !self.metered_opcodes.contains(&opcode) {
            return;
        }

        let entry = self.opcode_gas.entry((contract_address, opcode)).or_default();
        entry.count = entry.count.saturating_add(1);
        self.opcode_frame =
            Some(OpcodeFrame { contract_address, opcode, gas_remaining: interp.gas.remaining() });
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = context;

        if let Some(frame) = self.opcode_frame.take() {
            let gas_cost = frame.gas_remaining.saturating_sub(interp.gas.remaining());
            let entry = self.opcode_gas.entry((frame.contract_address, frame.opcode)).or_default();
            entry.gas_used = entry.gas_used.saturating_add(gas_cost);
        }
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let _ = (context, inputs);
        None
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let _ = context;

        let opcode = match inputs.scheme {
            CallScheme::Call => opcode::CALL,
            CallScheme::CallCode => opcode::CALLCODE,
            CallScheme::DelegateCall => opcode::DELEGATECALL,
            CallScheme::StaticCall => opcode::STATICCALL,
        };
        let contract_address = match inputs.scheme {
            CallScheme::Call | CallScheme::StaticCall => inputs.caller,
            CallScheme::CallCode | CallScheme::DelegateCall => inputs.target_address,
        };
        self.subtract_forwarded_gas(contract_address, opcode, inputs.gas_limit);

        let target = inputs.bytecode_address;
        if self.metered_precompiles.contains(&target) {
            let gas_used = outcome.result.gas.total_gas_spent();
            let entry = self.precompile_gas.entry(target).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.gas_used = entry.gas_used.saturating_add(gas_used);
        }
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let _ = (context, inputs);
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        let _ = context;

        let opcode = match inputs.scheme() {
            CreateScheme::Create => opcode::CREATE,
            CreateScheme::Create2 { .. } => opcode::CREATE2,
            CreateScheme::Custom { .. } => return,
        };
        self.subtract_forwarded_gas(inputs.caller(), opcode, inputs.gas_limit());
    }
}
