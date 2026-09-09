//! Core interpreter implementation and components.

/// Extended bytecode functionality.
pub mod ext_bytecode;
mod input;
mod runtime_flags;
mod shared_memory;
mod stack;

// re-exports
use base_execution_evm_machine::{GasParams, LoadError};
pub use ext_bytecode::ExtBytecode;
pub use input::InputsImpl;
use revm_bytecode::Bytecode;
use revm_primitives::{Bytes, hardfork::SpecId, hints_util::cold_path};
pub use runtime_flags::RuntimeFlags;
pub use shared_memory::{SharedMemory, num_words, resize_memory};
pub use stack::{STACK_LIMIT, Stack};

// imports
use crate::{
    Gas, GasTable, Host, InstructionExecResult, InstructionResult, InstructionTable,
    InterpreterAction, instruction_context::InstructionContext,
};

/// EVM interpreter with its stack, memory, bytecode, and execution state.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Interpreter {
    /// Bytecode being executed.
    pub bytecode: ExtBytecode,
    /// Gas tracking for execution costs.
    pub gas: Gas,
    /// EVM stack for computation.
    pub stack: Stack,
    /// Buffer for return data from calls.
    pub return_data: Bytes,
    /// EVM memory for data storage.
    pub memory: SharedMemory,
    /// Input data for current execution context.
    pub input: InputsImpl,
    /// Runtime flags controlling execution behavior.
    pub runtime_flag: RuntimeFlags,
}

impl Interpreter {
    /// Create new interpreter
    pub fn new(
        memory: SharedMemory,
        bytecode: ExtBytecode,
        input: InputsImpl,
        is_static: bool,
        spec_id: SpecId,
        gas_limit: u64,
    ) -> Self {
        Self::new_inner(Stack::new(), memory, bytecode, input, is_static, spec_id, gas_limit)
    }

    /// Create a new invalid interpreter.
    pub fn invalid() -> Self {
        Self::do_default(Stack::invalid(), SharedMemory::invalid())
    }

    fn do_default(stack: Stack, memory: SharedMemory) -> Self {
        Self::new_inner(
            stack,
            memory,
            ExtBytecode::default(),
            InputsImpl::default(),
            false,
            SpecId::default(),
            u64::MAX,
        )
    }

    fn new_inner(
        stack: Stack,
        memory: SharedMemory,
        bytecode: ExtBytecode,
        input: InputsImpl,
        is_static: bool,
        spec_id: SpecId,
        gas_limit: u64,
    ) -> Self {
        Self {
            bytecode,
            gas: Gas::new(gas_limit),
            stack,
            return_data: Default::default(),
            memory,
            input,
            runtime_flag: RuntimeFlags { is_static, spec_id },
        }
    }

    /// Clears and reinitializes the interpreter with new parameters.
    #[expect(clippy::too_many_arguments)]
    #[inline(always)]
    pub fn clear(
        &mut self,
        memory: SharedMemory,
        bytecode: ExtBytecode,
        input: InputsImpl,
        is_static: bool,
        spec_id: SpecId,
        gas_limit: u64,
        reservoir_remaining_gas: u64,
    ) {
        self.bytecode = bytecode;
        self.gas = Gas::new_with_regular_gas_and_reservoir(gas_limit, reservoir_remaining_gas);
        if self.stack.data().capacity() == 0 {
            self.stack = Stack::new();
        } else {
            self.stack.clear();
        }
        self.return_data.clear();
        self.memory = memory;
        self.input = input;
        self.runtime_flag = RuntimeFlags { spec_id, is_static };
    }

    /// Sets the bytecode that is going to be executed
    pub fn with_bytecode(mut self, bytecode: Bytecode) -> Self {
        self.bytecode = ExtBytecode::new(bytecode);
        self
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::do_default(Stack::new(), SharedMemory::new())
    }
}

impl Interpreter {
    /// Performs EVM memory resize.
    #[inline]
    pub fn resize_memory(
        &mut self,
        gas_params: &GasParams,
        offset: usize,
        len: usize,
    ) -> Result<(), InstructionResult> {
        resize_memory(&mut self.gas, &mut self.memory, gas_params, offset, len)
    }

    /// Takes the next action from the control and returns it.
    #[inline]
    pub fn take_next_action(&mut self) -> InterpreterAction {
        self.bytecode.reset_action();
        // Return next action if it is some.
        let action = core::mem::take(self.bytecode.action()).expect("Interpreter to set action");
        action
    }

    /// Halt the interpreter with the given result.
    ///
    /// This will set the action to [`InterpreterAction::Return`] and set the gas to the current gas.
    #[cold]
    #[inline(never)]
    pub fn halt(&mut self, result: InstructionResult) {
        if result == InstructionResult::OutOfGas {
            self.gas.spend_all();
        }
        self.bytecode.set_action(InterpreterAction::new_halt(result, self.gas));
    }

    /// Halt the interpreter with the given result.
    ///
    /// This will set the action to [`InterpreterAction::Return`] and set the gas to the current gas.
    #[cold]
    #[inline(never)]
    pub fn halt_fatal(&mut self) {
        self.bytecode.set_action(InterpreterAction::new_halt(
            InstructionResult::FatalExternalError,
            self.gas,
        ));
    }

    /// Halt the interpreter due to a [`LoadError`].
    #[cold]
    #[inline(never)]
    pub fn halt_load_error(&mut self, err: LoadError) {
        match err {
            LoadError::ColdLoadSkipped => self.halt_oog(),
            LoadError::DBError => self.halt_fatal(),
        }
    }

    /// Halt the interpreter with an out-of-gas error.
    #[cold]
    #[inline(never)]
    pub fn halt_oog(&mut self) {
        self.gas.spend_all();
        self.halt(InstructionResult::OutOfGas);
    }

    /// Halt the interpreter with an out-of-gas error.
    #[cold]
    #[inline(never)]
    pub fn halt_memory_oog(&mut self) {
        self.halt(InstructionResult::MemoryOOG);
    }

    /// Halt the interpreter with an out-of-gas error.
    #[cold]
    #[inline(never)]
    pub fn halt_memory_limit_oog(&mut self) {
        self.halt(InstructionResult::MemoryLimitOOG);
    }

    /// Halt the interpreter with and overflow error.
    #[cold]
    #[inline(never)]
    pub fn halt_overflow(&mut self) {
        self.halt(InstructionResult::StackOverflow);
    }

    /// Halt the interpreter with and underflow error.
    #[cold]
    #[inline(never)]
    pub fn halt_underflow(&mut self) {
        self.halt(InstructionResult::StackUnderflow);
    }

    /// Halt the interpreter with and not activated error.
    #[cold]
    #[inline(never)]
    pub fn halt_not_activated(&mut self) {
        self.halt(InstructionResult::NotActivated);
    }

    /// Return with the given output.
    ///
    /// This will set the action to [`InterpreterAction::Return`] and set the gas to the current gas.
    pub fn return_with_output(&mut self, output: Bytes) {
        self.bytecode.set_action(InterpreterAction::new_return(
            InstructionResult::Return,
            output,
            self.gas,
        ));
    }

    /// Executes the instruction at the current instruction pointer.
    ///
    /// Internally it will increment instruction pointer by one.
    #[inline]
    pub fn step<H: Host + ?Sized>(
        &mut self,
        instruction_table: &InstructionTable<H>,
        gas_table: &GasTable,
        host: &mut H,
    ) -> InstructionExecResult {
        // Get current opcode.
        let opcode = self.bytecode.opcode();

        // SAFETY: In analysis we are doing padding of bytecode so that we are sure that last
        // byte instruction is STOP so we are safe to just increment program_counter bcs on last instruction
        // it will do noop and just stop execution of this contract
        self.bytecode.relative_jump(1);

        let instruction = instruction_table[opcode as usize];
        let static_gas = unsafe { *gas_table.get_unchecked(opcode as usize) };

        if self.gas.record_cost_unsafe(static_gas as u64) {
            cold_path();
            return Err(InstructionResult::OutOfGas);
        }

        instruction.execute(InstructionContext { interpreter: self, host })
    }

    /// Executes the interpreter until it returns or stops.
    #[inline]
    pub fn run_plain<H: Host + ?Sized>(
        &mut self,
        instruction_table: &InstructionTable<H>,
        gas_table: &GasTable,
        host: &mut H,
    ) -> InterpreterAction {
        let e = loop {
            if let Err(e) = self.step(instruction_table, gas_table, host) {
                cold_path();
                break e;
            }
        };
        if self.bytecode.action().is_none() {
            self.halt(e);
        }
        debug_assert!(self.bytecode.is_end());
        self.take_next_action()
    }
}

/*
#[doc(hidden)]
#[unsafe(no_mangle)]
pub fn asm_run(
    interpreter: &mut Interpreter,
    host: &mut base_execution_evm_machine::DummyHost,
) {
    let table = crate::instruction_table();
    let gas_table = crate::gas_table();
    interpreter.run_plain(&table, &gas_table, host);
}
*/

/// The result of an interpreter operation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(::serde::Serialize, ::serde::Deserialize))]
pub struct InterpreterResult {
    /// The result of the instruction execution.
    pub result: InstructionResult,
    /// The output of the instruction execution.
    pub output: Bytes,
    /// The gas usage information.
    pub gas: Gas,
}

impl InterpreterResult {
    /// Returns a new `InterpreterResult` with the given values.
    pub const fn new(result: InstructionResult, output: Bytes, gas: Gas) -> Self {
        Self { result, output, gas }
    }

    /// Returns a new `InterpreterResult` for an out-of-gas error with the given gas limit.
    pub fn new_oog(gas_limit: u64, reservoir: u64) -> Self {
        Self {
            result: InstructionResult::OutOfGas,
            output: Bytes::default(),
            gas: Gas::new_spent_with_reservoir(gas_limit, reservoir),
        }
    }

    /// Returns whether the instruction result is a success.
    #[inline]
    pub const fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// Returns whether the instruction result is a revert.
    #[inline]
    pub const fn is_revert(&self) -> bool {
        self.result.is_revert()
    }

    /// Returns whether the instruction result is an error.
    #[deprecated(note = "use `is_halt` instead")]
    #[inline]
    pub const fn is_error(&self) -> bool {
        self.result.is_halt()
    }

    /// Returns whether the instruction result is a halt (error).
    #[inline]
    pub const fn is_halt(&self) -> bool {
        self.result.is_halt()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "serde")]
    fn test_interpreter_serde() {
        use revm_bytecode::Bytecode;
        use revm_primitives::Bytes;

        use super::*;

        let bytecode = Bytecode::new_raw(Bytes::from(&[0x60, 0x00, 0x60, 0x00, 0x01][..]));
        let interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            InputsImpl::default(),
            false,
            SpecId::default(),
            u64::MAX,
        );

        let serialized = serde_json::to_string_pretty(&interpreter).unwrap();
        let deserialized: Interpreter = serde_json::from_str(&serialized).unwrap();

        assert_eq!(
            interpreter.bytecode.pc(),
            deserialized.bytecode.pc(),
            "Program counter should be preserved"
        );
    }
}

#[test]
fn test_mstore_big_offset_memory_oog() {
    use revm_bytecode::Bytecode;
    use revm_primitives::Bytes;

    use super::*;
    use crate::{
        DummyHost,
        instructions::{gas_table, instruction_table},
    };

    let code = Bytes::from(
        &[
            0x60, 0x00, // PUSH1 0x00
            0x61, 0x27, 0x10, // PUSH2 0x2710  (10,000)
            0x52, // MSTORE
            0x00, // STOP
        ][..],
    );
    let bytecode = Bytecode::new_raw(code);

    let mut interpreter = Interpreter::new(
        SharedMemory::new(),
        ExtBytecode::new(bytecode),
        InputsImpl::default(),
        false,
        SpecId::default(),
        1000,
    );

    let table = instruction_table::<DummyHost>();
    let gas = gas_table();
    let mut host = DummyHost::default();
    let action = interpreter.run_plain(&table, &gas, &mut host);

    assert!(action.is_return());
    assert_eq!(action.instruction_result(), Some(InstructionResult::MemoryOOG));
}

#[test]
#[cfg(feature = "memory_limit")]
fn test_mstore_big_offset_memory_limit_oog() {
    use revm_bytecode::Bytecode;
    use revm_primitives::Bytes;

    use super::*;
    use crate::{
        DummyHost,
        instructions::{gas_table, instruction_table},
    };

    let code = Bytes::from(
        &[
            0x60, 0x00, // PUSH1 0x00
            0x61, 0x27, 0x10, // PUSH2 0x2710  (10,000)
            0x52, // MSTORE
            0x00, // STOP
        ][..],
    );
    let bytecode = Bytecode::new_raw(code);

    let mut interpreter = Interpreter::new(
        SharedMemory::new_with_memory_limit(1000),
        ExtBytecode::new(bytecode),
        InputsImpl::default(),
        false,
        SpecId::default(),
        100000,
    );

    let table = instruction_table::<DummyHost>();
    let gas = gas_table();
    let mut host = DummyHost::default();
    let action = interpreter.run_plain(&table, &gas, &mut host);

    assert!(action.is_return());
    assert_eq!(action.instruction_result(), Some(InstructionResult::MemoryLimitOOG));
}
