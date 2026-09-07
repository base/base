use alloc::string::ToString;
use alloy_primitives::map::HashMap;
use alloy_rpc_types_trace::opcode::OpcodeGas;
use revm::{
    bytecode::opcode::{self, OpCode},
    context::{ContextTr, JournalTr},
    interpreter::{
        interpreter_types::{Immediates, Jumps},
        CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, CreateScheme,
        Interpreter,
    },
    Inspector,
};

/// An Inspector that counts opcodes and measures gas usage per opcode.
#[derive(Clone, Debug, Default)]
pub struct OpcodeGasInspector {
    /// Map of opcode counts per transaction.
    opcode_counts: HashMap<OpCode, u64>,
    /// Map of total gas used per opcode.
    opcode_gas: HashMap<OpCode, u64>,
    /// Keep track of the last opcode executed and the remaining gas
    last_opcode_gas_remaining: Option<(OpCode, u64)>,
}

impl OpcodeGasInspector {
    /// Creates a new instance of the inspector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the opcode counts collected during transaction execution.
    pub const fn opcode_counts(&self) -> &HashMap<OpCode, u64> {
        &self.opcode_counts
    }

    /// Returns the opcode gas usage collected during transaction execution.
    pub const fn opcode_gas(&self) -> &HashMap<OpCode, u64> {
        &self.opcode_gas
    }

    /// Returns an iterator over all opcodes with their count and combined gas usage.
    ///
    /// Note: this returns in no particular order.
    pub fn opcode_iter(&self) -> impl Iterator<Item = (OpCode, (u64, u64))> + '_ {
        self.opcode_counts.iter().map(move |(&opcode, &count)| {
            let gas = self.opcode_gas.get(&opcode).copied().unwrap_or_default();
            (opcode, (count, gas))
        })
    }

    /// Returns an iterator over all opcodes with their count and combined gas usage.
    ///
    /// Note: this returns in no particular order.
    pub fn opcode_gas_iter(&self) -> impl Iterator<Item = OpcodeGas> + '_ {
        self.opcode_iter().map(|(opcode, (count, gas_used))| OpcodeGas {
            opcode: opcode.to_string(),
            count,
            gas_used,
        })
    }

    /// Helper function to subtract gas limit from opcode gas tracking.
    /// This prevents call/create opcodes from including the gas consumed within the call/create.
    fn subtract_gas_limit(&mut self, opcode_value: u8, gas_limit: u64) {
        if let Some(opcode) = OpCode::new(opcode_value) {
            let opcode_gas = self.opcode_gas.entry(opcode).or_default();
            *opcode_gas = opcode_gas.saturating_sub(gas_limit);
        }
    }
}

impl<CTX> Inspector<CTX> for OpcodeGasInspector
where
    CTX: ContextTr,
{
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        let opcode_value = interp.bytecode.opcode();
        if let Some(opcode) = OpCode::new(opcode_value) {
            // keep track of opcode counts
            *self.opcode_counts.entry(opcode).or_default() += 1;

            // keep track of the last opcode executed
            self.last_opcode_gas_remaining = Some((opcode, interp.gas.remaining()));
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        // update gas usage for the last opcode
        if let Some((opcode, gas_remaining)) = self.last_opcode_gas_remaining.take() {
            let gas_cost = gas_remaining.saturating_sub(interp.gas.remaining());
            *self.opcode_gas.entry(opcode).or_default() += gas_cost;
        }
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if context.journal_ref().depth() == 0 {
            // skip the root call
            return None;
        }

        // for accurate call opcode gas tracking, we need to deduct the gas limit from the opcode
        // gas, because otherwise the call opcodes would include the total gas consumed within the
        // call itself, but we want to track how much gas the call opcode itself consumes.
        let opcode = match inputs.scheme {
            CallScheme::Call => opcode::CALL,
            CallScheme::CallCode => opcode::CALLCODE,
            CallScheme::DelegateCall => opcode::DELEGATECALL,
            CallScheme::StaticCall => opcode::STATICCALL,
        };

        self.subtract_gas_limit(opcode, inputs.gas_limit);

        None
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if context.journal_ref().depth() == 0 {
            // skip the root create
            return None;
        }

        // for accurate create opcode gas tracking, we need to deduct the gas limit from the opcode
        // gas, because otherwise the create opcodes would include the total gas consumed within the
        // create itself, but we want to track how much gas the create opcode itself consumes.
        let opcode = match inputs.scheme() {
            CreateScheme::Create => opcode::CREATE,
            CreateScheme::Create2 { .. } => opcode::CREATE2,
            CreateScheme::Custom { .. } => return None,
        };

        self.subtract_gas_limit(opcode, inputs.gas_limit());

        None
    }
}

/// Accepts Bytecode that implements [Immediates] and returns the size of immediate
/// value.
///
/// Primarily needed to handle a special case of RJUMPV opcode.
pub fn immediate_size(bytecode: &impl Immediates) -> u8 {
    let opcode = bytecode.read_u8();
    let Some(opcode) = OpCode::new(opcode) else { return 0 };
    opcode.info().immediate_size()
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        bytecode::Bytecode,
        database::CacheDB,
        database_interface::EmptyDB,
        interpreter::{interpreter::ExtBytecode, InputsImpl, SharedMemory},
        primitives::{hardfork::SpecId, Bytes},
        Context, MainContext,
    };

    #[test]
    fn test_opcode_counter_inspector() {
        let mut opcode_counter = OpcodeGasInspector::new();

        let opcodes = [opcode::ADD, opcode::ADD, opcode::ADD, opcode::BYTE];

        let bytecode = Bytecode::new_raw(Bytes::from(opcodes));
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            InputsImpl::default(),
            false,
            SpecId::default(),
            u64::MAX,
        );
        let db = CacheDB::new(EmptyDB::default());

        let mut context = Context::mainnet().with_db(db);
        for _ in &opcodes {
            opcode_counter.step(&mut interpreter, &mut context);
        }
    }

    #[test]
    fn test_with_variety_of_opcodes() {
        let mut opcode_counter = OpcodeGasInspector::new();

        let opcodes = [
            opcode::PUSH1,
            opcode::PUSH1,
            opcode::ADD,
            opcode::PUSH1,
            opcode::SSTORE,
            opcode::STOP,
        ];

        let bytecode = Bytecode::new_raw(Bytes::from(opcodes));
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            InputsImpl::default(),
            false,
            SpecId::default(),
            u64::MAX,
        );
        let db = CacheDB::new(EmptyDB::default());

        let mut context = Context::mainnet().with_db(db);
        for _ in opcodes.iter() {
            opcode_counter.step(&mut interpreter, &mut context);
        }
    }
}
