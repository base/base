use alloc::{boxed::Box, vec::Vec};
use core::{fmt, ops::Range, ptr::NonNull};

use alloy_primitives::{Address, B256, Bytes};
use derive_where::derive_where;

use super::{
    BytecodeRef, Gas, InstrStop, Memory, Message, MessageKind, Pc, Result, StackBacking, StackMut,
    StackRef, Word,
};
use crate::{
    EvmTypesHost, ExecutionConfig, SpecId, Version,
    bytecode::Bytecode,
    env::TxEnv,
    evm::inspector::Inspector,
    interpreter::dispatch::{self, InstrTable},
    trustme,
    version::{EvmFeatures, GasParams},
};

/// EVM interpreter.
#[derive_where(Debug)]
pub struct Interpreter<'frame, 'host, T: EvmTypesHost> {
    pub(in crate::interpreter) bytecode: Bytecode,
    pub(in crate::interpreter) memory: Memory,
    pub(in crate::interpreter) return_data: Bytes,

    pub(in crate::interpreter) pc: *const u8,
    output: Range<u32>,
    #[derive_where(skip)]
    tx_env: Option<&'frame TxEnv<T>>,
    #[derive_where(skip)]
    message: Option<&'frame Message<T>>,
    host: Option<NonNull<T::Host<'host>>>,
    inspector: Option<NonNull<dyn Inspector<T> + 'host>>,
    version: Option<&'frame Version>,
    pub(in crate::interpreter) stack_len: usize,
    #[derive_where(skip)]
    pub(in crate::interpreter) stack: Box<StackBacking>,

    pub(in crate::interpreter) gas: Gas,
    pub(in crate::interpreter) result: Result,
    spec: SpecId,
    features: EvmFeatures,
    is_static: bool,
}

// SAFETY: The interpreter's internal pointers are always valid. `pc` points into owned bytecode,
// frame-local references are cleared before pooling, and host/inspector pointers are installed for
// execution and not used after the owning execution context is gone.
unsafe impl<T: EvmTypesHost> Send for Interpreter<'_, '_, T> {}

impl<'frame, 'host, T: EvmTypesHost> Interpreter<'frame, 'host, T> {
    /// Creates an interpreter from a transaction-global environment and a frame-local message,
    /// which carries the bytecode to run.
    pub fn new(tx_env: &'frame TxEnv<T>, message: &'frame Message<T>) -> Self {
        // SAFETY: Calling init right after.
        let mut interp = unsafe { Self::uninit() };
        interp.init(tx_env, message);
        interp
    }

    /// # Safety
    ///
    /// Must call `init` before use.
    unsafe fn uninit() -> Self {
        let bytecode = Bytecode::new();
        Self {
            pc: bytecode.original_byte_slice().as_ptr(),
            bytecode,
            stack_len: 0,
            gas: Gas::new(0),
            memory: Memory::new(),
            result: Ok(()),
            output: 0..0,
            tx_env: None,
            message: None,
            is_static: false,
            return_data: Bytes::new(),
            host: None,
            inspector: None,
            version: None,
            spec: SpecId::DEFAULT,
            features: EvmFeatures::empty(),
            // SAFETY: `MaybeUninit<Word>` does not need initialization.
            stack: unsafe { Box::new_uninit().assume_init() },
        }
    }

    /// Initializes this interpreter for a new frame, retaining reusable allocations.
    fn init(&mut self, tx_env: &'frame TxEnv<T>, message: &'frame Message<T>) {
        let bytecode = message.code.clone();
        let gas_limit = message.gas_limit;
        let is_static = message.caller_is_static || matches!(message.kind, MessageKind::StaticCall);
        self.pc = bytecode.original_byte_slice().as_ptr();
        self.bytecode = bytecode;
        self.stack_len = 0;
        self.gas = Gas::new_with_execution_gas_and_reservoir(gas_limit, message.reservoir);
        self.memory.clear();
        self.result = Ok(());
        self.output = 0..0;
        self.tx_env = Some(tx_env);
        self.message = Some(message);
        self.is_static = is_static;
        self.return_data = Bytes::new();
    }

    pub(crate) const fn clear_frame_refs(&mut self) {
        self.tx_env = None;
        self.message = None;
        self.version = None;
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (Box<StackBacking>, usize, Gas, Memory, Range<u32>) {
        (self.stack, self.stack_len, self.gas, self.memory, self.output)
    }

    /// Returns output produced by `RETURN` or `REVERT`.
    #[inline]
    pub fn output(&self) -> &[u8] {
        let start = self.output.start as usize;
        self.memory.slice(start, self.output.len())
    }

    /// Returns the current frame output memory range.
    #[inline]
    pub const fn output_range(&self) -> &Range<u32> {
        &self.output
    }

    /// Sets the current frame output memory range.
    #[inline]
    pub const fn set_output(&mut self, output: Range<u32>) {
        self.output = output;
    }

    /// Returns the current bytecode-relative program counter.
    #[inline]
    pub fn pc(&self) -> usize {
        // SAFETY: `pc` is always in bounds of `bytecode`.
        unsafe { self.pc.offset_from(self.bytecode.original_byte_slice().as_ptr()) as usize }
    }

    /// Sets the current bytecode-relative program counter.
    ///
    /// # Panics
    ///
    /// Panics if `pc` is out of bounds of the active bytecode.
    #[inline]
    pub fn set_pc(&mut self, pc: usize) {
        let bytecode = self.bytecode.bytes_slice();
        assert!(pc < bytecode.len());
        self.pc = unsafe { bytecode.as_ptr().add(pc) };
    }

    /// Returns the current opcode.
    #[inline]
    pub const fn opcode(&self) -> u8 {
        // SAFETY: `pc` is always in bounds of `bytecode`.
        unsafe { *self.pc }
    }

    /// Returns the active bytecode.
    #[inline]
    pub fn bytecode(&self) -> BytecodeRef<'_> {
        BytecodeRef::new(&self.bytecode)
    }

    /// Returns the original active bytecode bytes.
    #[inline]
    pub fn original_bytecode(&self) -> Bytes {
        self.bytecode.original_bytes()
    }

    /// Calculates or returns the cached hash of the original active bytecode.
    #[inline]
    pub fn original_bytecode_hash(&self) -> B256 {
        self.bytecode.hash_slow()
    }

    /// Returns the current operand stack.
    #[inline]
    pub const fn stack(&self) -> StackRef<'_> {
        StackRef::new(&self.stack, self.stack_len)
    }

    /// Returns the current mutable operand stack.
    #[inline]
    pub const fn stack_mut(&mut self) -> StackMut<'_> {
        StackMut { stack: &mut self.stack, len: &mut self.stack_len }
    }

    /// Returns the current gas state.
    #[inline]
    pub const fn gas(&self) -> Gas {
        self.gas
    }

    /// Returns a reference to the current gas state.
    #[inline]
    pub const fn gas_mut(&mut self) -> &mut Gas {
        &mut self.gas
    }

    /// Sets the current interpreter gas state.
    #[inline]
    pub const fn set_gas(&mut self, gas: Gas) {
        self.gas = gas;
    }

    /// Returns the current linear memory.
    #[inline]
    pub const fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Returns the current mutable linear memory.
    #[inline]
    pub const fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    /// Returns the current interpreter result.
    #[inline]
    pub const fn result(&self) -> Result {
        self.result
    }

    /// Sets the interpreter result.
    #[inline]
    pub const fn set_result(&mut self, result: Result) {
        self.result = result;
    }

    /// Sets the current instruction result to `stop`.
    #[inline]
    pub const fn set_stop(&mut self, stop: InstrStop) {
        self.set_result(Err(stop));
    }

    /// Returns the active frame-local call/create message.
    #[inline]
    pub const fn message(&self) -> &'frame Message<T> {
        // SAFETY: `message` is initialized before inspected execution starts.
        unsafe { self.message.unwrap_unchecked() }
    }

    #[inline]
    #[doc(hidden)]
    pub const fn message_field_offset_for_jit() -> usize {
        core::mem::offset_of!(Self, message)
    }

    /// Returns the cached transaction-global environment.
    #[inline]
    pub const fn tx_env(&self) -> &'frame TxEnv<T> {
        // SAFETY: `tx_env` is initialized before execution starts.
        unsafe { self.tx_env.unwrap_unchecked() }
    }

    /// Returns return data from the last call-like operation.
    #[inline]
    pub const fn return_data(&self) -> &Bytes {
        &self.return_data
    }

    /// Returns a mutable reference to return data from the last call-like operation.
    #[inline]
    pub const fn return_data_mut(&mut self) -> &mut Bytes {
        &mut self.return_data
    }

    /// Returns the host implementation.
    #[inline]
    pub const fn host(&mut self) -> &mut T::Host<'host> {
        // SAFETY: `host` is initialized at the beginning of inspected execution.
        unsafe { self.host.unwrap_unchecked().as_mut() }
    }

    /// Returns the active base specification ID.
    #[inline]
    pub const fn spec(&self) -> SpecId {
        self.spec
    }

    /// Returns the active runtime version data.
    #[inline]
    pub const fn version(&self) -> &Version {
        // SAFETY: `version` is initialized before execution starts.
        unsafe { self.version.unwrap_unchecked() }
    }

    /// Returns whether the active frame forbids state-changing operations.
    #[inline]
    pub const fn is_static(&self) -> bool {
        self.is_static
    }

    /// Sets the static-call flag for external execution adapters.
    #[inline]
    pub const fn set_static(&mut self, is_static: bool) {
        self.is_static = is_static;
    }

    /// Runs the interpreter until it stops.
    #[inline]
    pub fn run(&mut self, config: &ExecutionConfig<T>, host: &mut T::Host<'host>) -> InstrStop {
        self.run_inner(config.base_spec_id(), config.version(), host, None, config.instructions)
    }

    /// Runs the interpreter until it stops with an execution inspector.
    #[inline]
    pub fn run_inspect(
        &mut self,
        config: &ExecutionConfig<T>,
        host: &mut T::Host<'host>,
        inspector: &mut (dyn Inspector<T> + 'host),
    ) -> InstrStop {
        self.run_inner(
            config.base_spec_id(),
            config.version(),
            host,
            Some(NonNull::from(inspector)),
            config.inspect_instructions,
        )
    }

    /// Prepares this interpreter for external execution.
    #[inline]
    #[doc(hidden)]
    pub fn prepare_run(&mut self, spec: SpecId, version: &Version, host: &mut T::Host<'host>) {
        self.memory.set_memory_limit(version.memory_limit);
        // SAFETY: `version` remains alive for the duration of this interpreter run.
        let version = unsafe { trustme::decouple_lt(version) };
        self.host = Some(NonNull::from(host));
        self.inspector = None;
        self.version = Some(version);
        self.spec = spec;
        self.features = version.features;
    }

    #[inline(never)]
    fn run_inner(
        &mut self,
        spec: SpecId,
        version: &Version,
        host: &mut T::Host<'host>,
        inspector: Option<NonNull<dyn Inspector<T> + 'host>>,
        instructions: &InstrTable<T>,
    ) -> InstrStop {
        self.prepare_run(spec, version, host);
        self.inspector = inspector;

        dispatch::run(self, instructions)
    }
}

/// Interpreter state exposed to instruction implementations.
#[repr(transparent)]
pub struct InterpreterState<'frame, 'host, T: EvmTypesHost>(
    pub(crate) Interpreter<'frame, 'host, T>,
);

impl<T: EvmTypesHost> fmt::Debug for InterpreterState<'_, '_, T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'frame, 'host, T: EvmTypesHost> InterpreterState<'frame, 'host, T> {
    #[inline]
    pub(crate) const fn wrap_mut<'a>(
        interp: &'a mut Interpreter<'frame, 'host, T>,
    ) -> &'a mut Self {
        // SAFETY: `InterpreterState` is a transparent wrapper over `Interpreter`.
        unsafe { core::mem::transmute::<&mut Interpreter<'frame, 'host, T>, &mut Self>(interp) }
    }

    #[inline]
    pub(in crate::interpreter) const unsafe fn gas_from_state_ptr(state: *mut Self) -> *mut Gas {
        // SAFETY: The caller upholds that `state` points to a valid interpreter state.
        unsafe { &raw mut (*state).0.gas }
    }

    #[inline]
    pub(crate) const fn gas_mut(&mut self) -> &mut Gas {
        &mut self.0.gas
    }

    #[inline(always)]
    pub(crate) const fn set_result(&mut self, result: Result) {
        self.0.result = result;
    }

    #[inline]
    pub(crate) const fn result(&self) -> Result {
        self.0.result
    }

    #[inline]
    #[cfg(not(tco))]
    pub(crate) const fn is_inspecting(&self) -> bool {
        self.0.inspector.is_some()
    }

    #[inline]
    pub(crate) const fn set_pc_stack_len(&mut self, pc: *const u8, stack_len: usize) {
        self.0.pc = pc;
        self.0.stack_len = stack_len;
    }

    /// Returns the cached transaction-global environment.
    #[inline]
    pub const fn tx(&self) -> &'frame TxEnv<T> {
        // SAFETY: `tx_env` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.tx_env.unwrap_unchecked() }
    }

    /// Returns the active base specification ID.
    #[inline]
    pub const fn spec(&self) -> SpecId {
        self.0.spec
    }

    /// Returns the active bytecode.
    #[inline]
    pub fn bytecode(&self) -> BytecodeRef<'_> {
        BytecodeRef::new(&self.0.bytecode)
    }

    /// Returns the host implementation.
    #[inline]
    pub const fn host(&mut self) -> &mut T::Host<'host> {
        // SAFETY: `host` is initialized at the beginning of `run` and cleared before the
        // method returns. Instruction execution is synchronous, so the pointer cannot outlive the
        // `run` host borrow.
        unsafe { self.0.host.unwrap_unchecked().as_mut() }
    }

    /// Returns the active runtime version data.
    #[inline]
    pub const fn version(&self) -> &'frame Version {
        // SAFETY: `version` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.version.unwrap_unchecked() }
    }

    /// Returns the active frame-local call/create message.
    #[inline]
    pub const fn message(&self) -> &'frame Message<T> {
        // SAFETY: `message` is initialized at the beginning of `run` and remains set for
        // instruction execution.
        unsafe { self.0.message.unwrap_unchecked() }
    }

    /// Returns whether the active frame forbids state-changing operations.
    #[inline]
    pub const fn is_static(&self) -> bool {
        self.0.is_static
    }

    /// Returns `true` if the active feature set contains `feature`.
    #[inline]
    pub const fn feature(&self, feature: EvmFeatures) -> bool {
        self.0.features.contains(feature)
    }

    /// Returns the active dynamic gas parameters.
    #[inline]
    pub const fn gas_params(&self) -> &'frame GasParams {
        &self.version().gas_params
    }

    /// Returns linear memory.
    #[inline]
    pub const fn memory(&mut self) -> &mut Memory {
        &mut self.0.memory
    }

    /// Resizes linear memory using the active runtime gas parameters.
    #[inline]
    pub fn resize_memory(&mut self, gas: &mut Gas, offset: usize, len: usize) -> Result {
        self.0.memory.resize_evm(gas, self.gas_params(), offset, len)
    }

    /// Returns return data from the last call-like operation.
    #[inline]
    pub const fn return_data(&self) -> &Bytes {
        &self.0.return_data
    }

    /// Returns a mutable reference to return data from the last call-like operation.
    #[inline]
    pub const fn return_data_mut(&mut self) -> &mut Bytes {
        &mut self.0.return_data
    }

    /// Clears return data from the last call-like operation.
    #[inline]
    pub fn clear_return_data(&mut self) {
        self.0.return_data.clear();
    }

    /// Swaps return data from the last call-like operation.
    #[inline]
    pub(crate) const fn swap_return_data(&mut self, return_data: &mut Bytes) {
        core::mem::swap(&mut self.0.return_data, return_data);
    }

    /// Returns the current frame output memory range.
    #[inline]
    pub const fn output_range(&self) -> &Range<u32> {
        &self.0.output
    }

    /// Sets the current frame output memory range.
    #[inline]
    pub const fn set_output(&mut self, output: Range<u32>) {
        self.0.output = output;
    }

    #[inline]
    pub(crate) fn inspect_step(&mut self, pc: Pc, stack_len: usize) {
        self.0.pc = pc.as_ptr();
        self.0.stack_len = stack_len;
        unsafe {
            let mut inspector = self.0.inspector.unwrap_unchecked();
            inspector.as_mut().step(&mut self.0);
        }
    }

    #[inline]
    pub(crate) fn inspect_step_end(&mut self, pc: Pc, stack_len: usize) {
        self.0.pc = pc.as_ptr();
        self.0.stack_len = stack_len;
        unsafe {
            let mut inspector = self.0.inspector.unwrap_unchecked();
            inspector.as_mut().step_end(&mut self.0);
        }
    }

    #[inline]
    pub(crate) fn inspect_selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &Word,
    ) {
        if let Some(mut inspector) = self.0.inspector {
            unsafe {
                let mut host = self.0.host.unwrap_unchecked();
                inspector.as_mut().selfdestruct(contract, target, value, host.as_mut());
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct InterpreterPool<T: EvmTypesHost> {
    frames: Vec<Box<Interpreter<'static, 'static, T>>>,
}

impl<T: EvmTypesHost> InterpreterPool<T> {
    pub(crate) const fn new() -> Self {
        Self { frames: Vec::new() }
    }

    pub(crate) fn pop<'frame, 'host>(
        &mut self,
        tx_env: &'frame TxEnv<T>,
        message: &'frame Message<T>,
    ) -> Box<Interpreter<'frame, 'host, T>> {
        let frame = match self.frames.pop() {
            Some(mut frame) => {
                frame.init(tx_env, message);
                frame
            }
            None => Box::new(Interpreter::new(tx_env, message)),
        };
        // SAFETY: Frames stored in the pool have their frame-local references cleared before they
        // are erased to `'static`. Rebinding the lifetime is only used to initialize the next
        // frame.
        unsafe { trustme::decouple_lt_box(frame) }
    }

    pub(crate) fn push<'pool, 'frame, 'host>(
        &'pool mut self,
        mut frame: Box<Interpreter<'frame, 'host, T>>,
    ) -> &'pool mut Interpreter<'frame, 'host, T> {
        frame.clear_frame_refs();
        // SAFETY: `clear_frame_refs` removes every reference carrying `'frame`, so the boxed
        // interpreter can be stored in the pool with the erased `'static` lifetime.
        let frame = unsafe { trustme::decouple_lt_box(frame) };
        let frame = self.frames.push_mut(frame);
        // SAFETY: The returned borrow is tied to `&mut self`; the erased frame references are
        // empty.
        unsafe { trustme::decouple_interpreter_lt_mut(frame) }
    }

    pub(crate) fn last_mut<'frame, 'host>(&mut self) -> Option<&mut Interpreter<'frame, 'host, T>> {
        let frame = self.frames.last_mut()?.as_mut();
        // SAFETY: Frames stored in the pool have had their frame-local references cleared by
        // `push`, and this borrow is tied to the pool borrow.
        Some(unsafe { trustme::decouple_interpreter_lt_mut(frame) })
    }
}
