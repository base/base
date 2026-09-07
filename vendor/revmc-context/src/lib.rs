#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::{
    fmt,
    mem::MaybeUninit,
    ptr::{self, NonNull},
};

use revm_interpreter::{
    Gas, Host, InputsImpl, InstructionResult, Interpreter, InterpreterAction, InterpreterResult,
    SharedMemory,
    context_interface::cfg::GasParams,
    interpreter_types::{Jumps, LegacyBytecode, ReturnData, RuntimeFlag},
};
use revm_primitives::{Address, B256, Bytes, Log, U256, hardfork::SpecId, ruint};

mod arch;
use arch::revmc_entry;
pub use arch::revmc_exit;

#[cfg(feature = "evm")]
mod jit_evm;
#[cfg(feature = "evm")]
pub use jit_evm::JitEvm;

/// Resume point for compiled EVM code after a CALL/CREATE suspension.
///
/// Encoded as the interpreter's bytecode PC. `0` means no resume (initial state),
/// values `1..=N` identify individual suspend points.
///
/// Since the number of suspend points cannot exceed the bytecode length, the stored
/// PC always stays within the allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
#[doc(hidden)] // Not public API.
pub struct ResumeAt(usize);

impl ResumeAt {
    /// Loads the resume point from the interpreter's current PC.
    #[inline]
    pub fn load(interpreter: &Interpreter) -> Self {
        Self(interpreter.bytecode.pc())
    }

    /// Stores the resume point back into the interpreter's bytecode PC.
    #[inline]
    pub fn store(self, interpreter: &mut Interpreter) {
        interpreter.bytecode.absolute_jump(self.0);
    }

    /// Returns the raw resume index.
    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for ResumeAt {
    #[inline]
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl PartialEq<usize> for ResumeAt {
    #[inline]
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

/// The EVM bytecode compiler runtime context.
///
/// This is a simple wrapper around the interpreter's resources, allowing the compiled function to
/// access the memory, input, gas, host, and other resources.
///
/// # Safety
/// This struct uses `#[repr(C)]` to ensure a stable field layout since the JIT compiler
/// generates code that accesses fields by offset using `offset_of!`.
#[repr(C)]
pub struct EvmContext<'a> {
    /// The memory.
    pub memory: &'a mut SharedMemory,
    /// Input information (target address, caller, input data, call value).
    pub input: &'a mut InputsImpl,
    /// The gas.
    pub gas: Gas,
    /// The host.
    pub host: &'a mut dyn Host,
    /// The return action.
    pub next_action: &'a mut Option<InterpreterAction>,
    /// The return data.
    pub return_data: &'a [u8],
    /// Whether the context is static.
    pub is_static: bool,
    /// The spec ID for the current execution.
    pub spec_id: SpecId,
    /// Index that tracks where execution should resume after a CALL/CREATE suspension.
    #[doc(hidden)] // Not public API.
    pub resume_at: ResumeAt,
    /// The contract bytecode, for CODECOPY at runtime.
    pub bytecode: *const [u8],
    /// Optional callback invoked by the LOG builtin after constructing the log,
    /// **before** it is passed to [`Host::log`].
    ///
    /// Set to `None` when no inspector is active.
    #[doc(hidden)]
    pub on_log: Option<&'a mut (dyn FnMut(&Log) + 'a)>,
    /// The size of the call input data, cached for CALLDATASIZE.
    pub calldatasize: usize,
    /// The result set by a builtin before exiting via [`revmc_exit`].
    pub exit_result: InstructionResult,
    /// Saved RSP from the entry trampoline, used by [`revmc_exit`] to unwind.
    pub exit_sp: *mut u8,
    /// Cached gas parameters from the host.
    pub gas_params: GasParams,
    /// Cached base pointer for the current memory context.
    /// Points to `memory[checkpoint..]`, i.e. the start of the current context's memory.
    /// Refreshed after any memory resize.
    pub mem_base: *mut u8,
    /// Cached length of the current memory context in bytes.
    /// Refreshed after any memory resize.
    pub mem_len: usize,
}

// Static assertions to ensure the struct layout matches expectations.
// These offsets are used by the JIT compiler to access fields.
const _: () = {
    use core::mem::offset_of;

    // Key fields accessed by JIT code
    assert!(offset_of!(EvmContext<'_>, memory) == 0);
    assert!(offset_of!(EvmContext<'_>, gas) == 16);
    assert!(offset_of!(EvmContext<'_>, spec_id) == 121);
    assert!(offset_of!(EvmContext<'_>, resume_at) == 128);
    assert!(offset_of!(EvmContext<'_>, calldatasize) == 168);
};

impl fmt::Debug for EvmContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvmContext").field("memory", &self.memory).finish_non_exhaustive()
    }
}

impl<'a> EvmContext<'a> {
    /// Creates a new context from an interpreter.
    #[inline]
    pub fn from_interpreter(interpreter: &'a mut Interpreter, host: &'a mut dyn Host) -> Self {
        Self::from_interpreter_with_stack(interpreter, host).0
    }

    /// Creates a new context from an interpreter.
    #[inline]
    pub fn from_interpreter_with_stack<'b: 'a>(
        interpreter: &'a mut Interpreter,
        host: &'b mut dyn Host,
    ) -> (Self, &'a mut EvmStack, &'a mut usize) {
        let resume_at = ResumeAt::load(interpreter);
        let (stack, stack_len) = EvmStack::from_interpreter_stack(&mut interpreter.stack);
        let bytecode = interpreter.bytecode.bytecode_slice() as *const [u8];
        let calldatasize = interpreter.input.input.len();
        let gas_params = host.gas_params().clone();
        let mut this = Self {
            memory: &mut interpreter.memory,
            input: &mut interpreter.input,
            gas: interpreter.gas,
            host,
            next_action: &mut interpreter.bytecode.action,
            return_data: interpreter.return_data.buffer(),
            is_static: interpreter.runtime_flag.is_static(),
            spec_id: interpreter.runtime_flag.spec_id(),
            resume_at,
            bytecode,
            on_log: None,
            calldatasize,
            exit_result: InstructionResult::Stop,
            exit_sp: ptr::null_mut(),
            gas_params,
            mem_base: ptr::null_mut(),
            mem_len: 0,
        };
        this.refresh_memory_cache();
        (this, stack, stack_len)
    }

    /// Refreshes the cached memory base pointer and length from `SharedMemory`.
    ///
    /// Must be called after any operation that may resize memory.
    #[inline]
    pub fn refresh_memory_cache(&mut self) {
        let mut slice = self.memory.context_memory_mut();
        self.mem_base = slice.as_mut_ptr();
        self.mem_len = slice.len();
    }
}

/// Declare [`RawEvmCompilerFn`] functions in an `extern "C"` block.
///
/// # Examples
///
/// ```no_run
/// use revmc_context::{EvmCompilerFn, extern_revmc};
///
/// extern_revmc! {
///    /// A simple function.
///    pub fn test_fn;
/// }
///
/// let test_fn = EvmCompilerFn::new(test_fn);
/// ```
#[macro_export]
macro_rules! extern_revmc {
    ($( $(#[$attr:meta])* $vis:vis fn $name:ident; )+) => {
        #[allow(improper_ctypes)]
        unsafe extern "C" {
            $(
                $(#[$attr])*
                $vis fn $name(
                    ecx: ::core::ptr::NonNull<$crate::EvmContext<'_>>,
                    stack: ::core::ptr::NonNull<$crate::EvmStack>,
                    stack_len: ::core::ptr::NonNull<usize>,
                ) -> $crate::private::revm_interpreter::InstructionResult;
            )+
        }
    };
}

/// The raw function signature of a bytecode function.
///
/// Prefer using [`EvmCompilerFn`] instead of this type. See [`EvmCompilerFn::call`] for more
/// information.
// When changing the signature, also update the corresponding declarations in `fn translate`.
pub type RawEvmCompilerFn = unsafe extern "C" fn(
    ecx: NonNull<EvmContext<'_>>,
    stack: NonNull<EvmStack>,
    stack_len: NonNull<usize>,
) -> InstructionResult;

/// An EVM bytecode function.
#[derive(Clone, Copy, Debug, Hash)]
pub struct EvmCompilerFn(RawEvmCompilerFn);

impl From<RawEvmCompilerFn> for EvmCompilerFn {
    #[inline]
    fn from(f: RawEvmCompilerFn) -> Self {
        Self::new(f)
    }
}

impl From<EvmCompilerFn> for RawEvmCompilerFn {
    #[inline]
    fn from(f: EvmCompilerFn) -> Self {
        f.into_inner()
    }
}

impl EvmCompilerFn {
    /// Wraps the function.
    #[inline]
    pub const fn new(f: RawEvmCompilerFn) -> Self {
        Self(f)
    }

    /// Unwraps the function.
    #[inline]
    pub const fn into_inner(self) -> RawEvmCompilerFn {
        self.0
    }

    /// Calls the function by re-using the interpreter's resources.
    ///
    /// This behaves similarly to `Interpreter::run_plain`, returning an [`InstructionResult`]
    /// and the next action in an [`InterpreterAction`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the function is safe to call.
    pub unsafe fn call_with_interpreter(
        self,
        interpreter: &mut Interpreter,
        host: &mut dyn Host,
    ) -> InterpreterAction {
        self.call_with_interpreter_inner(interpreter, host, |_| {})
    }

    /// Like [`call_with_interpreter`](Self::call_with_interpreter), but calls `configure` on the
    /// [`EvmContext`] before invoking the compiled function.
    ///
    /// This can be used to install callbacks (e.g. [`EvmContext::on_log`]) that fire during
    /// execution.
    ///
    /// # Safety
    ///
    /// Same requirements as [`call_with_interpreter`](Self::call_with_interpreter).
    #[doc(hidden)]
    pub unsafe fn call_with_interpreter_with(
        self,
        interpreter: &mut Interpreter,
        host: &mut dyn Host,
        configure: impl FnOnce(&mut EvmContext<'_>),
    ) -> InterpreterAction {
        self.call_with_interpreter_inner(interpreter, host, configure)
    }

    unsafe fn call_with_interpreter_inner(
        self,
        interpreter: &mut Interpreter,
        host: &mut dyn Host,
        configure: impl FnOnce(&mut EvmContext<'_>),
    ) -> InterpreterAction {
        interpreter.bytecode.action = None;

        let (mut ecx, stack, stack_len) =
            EvmContext::from_interpreter_with_stack(interpreter, host);
        configure(&mut ecx);
        let result = self.call(stack, stack_len, &mut ecx);

        let resume_at = ecx.resume_at;

        // Set the remaining gas to 0 if the result is `OutOfGas`,
        // as it might have overflown inside of the function.
        if result == InstructionResult::OutOfGas {
            ecx.gas.spend_all();
        }

        let return_data_is_empty = ecx.return_data.is_empty();
        interpreter.gas = ecx.gas;

        if return_data_is_empty {
            interpreter.return_data.0.clear();
        }

        resume_at.store(interpreter);

        if let Some(action) = interpreter.bytecode.action.take() {
            action
        } else {
            InterpreterAction::Return(InterpreterResult {
                result,
                output: Bytes::new(),
                gas: interpreter.gas,
            })
        }
    }

    /// Calls the function.
    ///
    /// Arguments:
    /// - `stack`: The stack buffer.
    /// - `stack_len`: The stack length.
    /// - `ecx`: The context object.
    ///
    /// Use of this method is discouraged, as setup and cleanup need to be done manually.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the arguments are valid and that the function is safe to call.
    #[inline]
    pub unsafe fn call(
        self,
        stack: &mut EvmStack,
        stack_len: &mut usize,
        ecx: &mut EvmContext<'_>,
    ) -> InstructionResult {
        revmc_entry(NonNull::from(ecx), NonNull::from(stack), NonNull::from(stack_len), self.0)
    }

    /// Same as [`call`](Self::call) but with `#[inline(never)]`.
    ///
    /// Use of this method is discouraged, as setup and cleanup need to be done manually.
    ///
    /// # Safety
    ///
    /// See [`call`](Self::call).
    #[inline(never)]
    pub unsafe fn call_noinline(
        self,
        stack: &mut EvmStack,
        stack_len: &mut usize,
        ecx: &mut EvmContext<'_>,
    ) -> InstructionResult {
        self.call(stack, stack_len, ecx)
    }
}

/// EVM context stack.
#[repr(C)]
#[allow(missing_debug_implementations)]
pub struct EvmStack([MaybeUninit<EvmWord>; 1024]);

#[allow(clippy::new_without_default)]
impl EvmStack {
    /// The size of the stack in bytes.
    pub const SIZE: usize = 32 * Self::CAPACITY;

    /// The size of the stack in U256 elements.
    pub const CAPACITY: usize = 1024;

    /// Creates a new EVM stack, allocated on the stack.
    ///
    /// Use [`EvmStack::new_heap`] to create a stack on the heap.
    #[inline]
    pub fn new() -> Self {
        Self(unsafe { MaybeUninit::uninit().assume_init() })
    }

    /// Creates a vector that can be used as a stack.
    #[inline]
    pub fn new_heap() -> Vec<EvmWord> {
        Vec::with_capacity(1024)
    }

    /// Creates a stack from the interpreter's stack. Assumes that the stack is large enough.
    #[inline]
    pub fn from_interpreter_stack(stack: &mut revm_interpreter::Stack) -> (&mut Self, &mut usize) {
        debug_assert!(stack.data().capacity() >= Self::CAPACITY);
        let expected_len = stack.len();
        unsafe {
            let data = Self::from_mut_ptr(stack.data_mut().as_mut_ptr().cast());
            // Vec { data: ptr, cap: usize, len: usize }
            let len = &mut *(stack.data_mut() as *mut Vec<_>).cast::<usize>().add(2);
            debug_assert_eq!(expected_len, *len);
            (data, len)
        }
    }

    /// Creates a stack from a vector's buffer.
    ///
    /// # Panics
    ///
    /// Panics if the vector's capacity is less than the required stack capacity.
    #[inline]
    pub fn from_vec(vec: &Vec<EvmWord>) -> &Self {
        assert!(vec.capacity() >= Self::CAPACITY);
        unsafe { Self::from_ptr(vec.as_ptr()) }
    }

    /// Creates a stack from a mutable vector's buffer.
    ///
    /// # Panics
    ///
    /// Panics if the vector's capacity is less than the required stack capacity.
    #[inline]
    pub fn from_mut_vec(vec: &mut Vec<EvmWord>) -> &mut Self {
        assert!(vec.capacity() >= Self::CAPACITY);
        unsafe { Self::from_mut_ptr(vec.as_mut_ptr()) }
    }

    /// Creates a stack from a pointer to a buffer.
    ///
    /// # Safety
    ///
    /// See [`from_vec`](Self::from_vec).
    #[inline]
    pub unsafe fn from_ptr<'a>(ptr: *const EvmWord) -> &'a Self {
        debug_assert!(ptr.is_aligned());
        unsafe { &*ptr.cast::<Self>() }
    }

    /// Creates a stack from a mutable pointer to a buffer.
    ///
    /// # Safety
    ///
    /// See [`from_mut_vec`](Self::from_mut_vec).
    #[inline]
    pub unsafe fn from_mut_ptr<'a>(ptr: *mut EvmWord) -> &'a mut Self {
        debug_assert!(ptr.is_aligned());
        unsafe { &mut *ptr.cast::<Self>() }
    }

    /// Returns a pointer to the stack.
    #[inline]
    pub const fn as_ptr(&self) -> *const EvmWord {
        self.0.as_ptr().cast()
    }

    /// Returns a mutable pointer to the stack.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut EvmWord {
        self.0.as_mut_ptr().cast()
    }

    /// Returns a slice of the initialized portion of the stack.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the first `len` slots are initialized.
    #[inline]
    pub unsafe fn as_slice(&self, len: usize) -> &[EvmWord] {
        assert!(len <= Self::CAPACITY);
        unsafe { core::slice::from_raw_parts(self.as_ptr(), len) }
    }

    /// Returns a mutable slice of the initialized portion of the stack.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the first `len` slots are initialized.
    #[inline]
    pub unsafe fn as_mut_slice(&mut self, len: usize) -> &mut [EvmWord] {
        assert!(len <= Self::CAPACITY);
        unsafe { core::slice::from_raw_parts_mut(self.as_mut_ptr(), len) }
    }

    /// Sets the value at the given index.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    #[inline]
    pub fn set(&mut self, index: usize, value: EvmWord) {
        self.0[index] = MaybeUninit::new(value);
    }

    /// Returns the word at the given index as a reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the slot at `index` is initialized.
    #[inline]
    pub unsafe fn get(&self, index: usize) -> Option<&EvmWord> {
        self.0.get(index).map(|slot| unsafe { slot.assume_init_ref() })
    }

    /// Returns the word at the given index as a mutable reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the slot at `index` is initialized.
    #[inline]
    pub unsafe fn get_mut(&mut self, index: usize) -> Option<&mut EvmWord> {
        self.0.get_mut(index).map(|slot| unsafe { slot.assume_init_mut() })
    }

    /// Returns the word at the given index as a reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the index is within bounds.
    #[inline]
    pub unsafe fn get_unchecked(&self, index: usize) -> &EvmWord {
        self.0.get_unchecked(index).assume_init_ref()
    }

    /// Returns the word at the given index as a mutable reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the index is within bounds.
    #[inline]
    pub unsafe fn get_unchecked_mut(&mut self, index: usize) -> &mut EvmWord {
        self.0.get_unchecked_mut(index).assume_init_mut()
    }

    /// Sets the value at the top of the stack to `value`, and grows the stack by 1.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the stack is not full.
    #[inline]
    pub unsafe fn push(&mut self, value: EvmWord, len: &mut usize) {
        self.set_unchecked(*len, value);
        *len += 1;
    }

    /// Returns the value at the top of the stack.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the stack is not empty.
    #[inline]
    pub unsafe fn top_unchecked(&self, len: usize) -> &EvmWord {
        self.get_unchecked(len - 1)
    }

    /// Returns the value at the top of the stack as a mutable reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the stack is not empty.
    #[inline]
    pub unsafe fn top_unchecked_mut(&mut self, len: usize) -> &mut EvmWord {
        self.get_unchecked_mut(len - 1)
    }

    /// Returns the value at the given index from the top of the stack.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `len >= n + 1`.
    #[inline]
    pub unsafe fn from_top_unchecked(&self, len: usize, n: usize) -> &EvmWord {
        self.get_unchecked(len - n - 1)
    }

    /// Returns the value at the given index from the top of the stack as a mutable reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `len >= n + 1`.
    #[inline]
    pub unsafe fn from_top_unchecked_mut(&mut self, len: usize, n: usize) -> &mut EvmWord {
        self.get_unchecked_mut(len - n - 1)
    }

    /// Sets the value at the given index.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the index is within bounds.
    #[inline]
    pub unsafe fn set_unchecked(&mut self, index: usize, value: EvmWord) {
        *self.0.get_unchecked_mut(index) = MaybeUninit::new(value);
    }
}

/// An EVM stack word, which is stored in native-endian order.
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EvmWord(B256);

impl Default for EvmWord {
    #[inline]
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for EvmWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_u256().fmt(f)
    }
}

impl fmt::Display for EvmWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_u256().fmt(f)
    }
}

impl TryFrom<EvmWord> for usize {
    type Error = ruint::FromUintError<Self>;

    #[inline]
    fn try_from(w: EvmWord) -> Result<Self, Self::Error> {
        Self::try_from(&w)
    }
}

impl TryFrom<&EvmWord> for usize {
    type Error = ruint::FromUintError<Self>;

    #[inline]
    fn try_from(w: &EvmWord) -> Result<Self, Self::Error> {
        w.to_u256().try_into()
    }
}

impl TryFrom<&mut EvmWord> for usize {
    type Error = ruint::FromUintError<Self>;

    #[inline]
    fn try_from(w: &mut EvmWord) -> Result<Self, Self::Error> {
        Self::try_from(&*w)
    }
}

impl From<U256> for EvmWord {
    #[inline]
    fn from(u: U256) -> Self {
        Self::from_u256(u)
    }
}

impl EvmWord {
    /// Zero.
    pub const ZERO: Self = Self(B256::ZERO);

    /// Create a new word from big-endian bytes.
    #[inline]
    pub const fn from_be_bytes(bytes: B256) -> Self {
        Self::from_be(Self(bytes))
    }

    /// Create a new word from big-endian bytes.
    #[inline]
    pub const fn from_be_slice(bytes: &[u8]) -> Self {
        Self::from_u256(U256::from_be_slice(bytes))
    }

    /// Create a new word from little-endian bytes.
    #[inline]
    pub const fn from_le_bytes(bytes: B256) -> Self {
        Self::from_le(Self(bytes))
    }

    /// Create a new word from little-endian slice.
    #[inline]
    pub const fn from_le_slice(bytes: &[u8]) -> Self {
        Self::from_u256(U256::from_le_slice(bytes))
    }

    /// Create a new word from native-endian bytes.
    #[inline]
    pub const fn from_ne_bytes(bytes: B256) -> Self {
        Self(bytes)
    }

    /// Create a new word from a [`U256`]. This is a no-op on little-endian systems.
    #[inline]
    pub const fn from_u256(u: U256) -> Self {
        #[cfg(target_endian = "little")]
        return unsafe { core::mem::transmute::<U256, Self>(u) };
        #[cfg(target_endian = "big")]
        return Self(B256::new(u.to_be_bytes()));
    }

    /// Converts a big-endian representation into a native one.
    #[inline]
    pub const fn from_be(x: Self) -> Self {
        #[cfg(target_endian = "little")]
        return x.swap_bytes();
        #[cfg(target_endian = "big")]
        return x;
    }

    /// Converts a little-endian representation into a native one.
    #[inline]
    pub const fn from_le(x: Self) -> Self {
        #[cfg(target_endian = "little")]
        return x;
        #[cfg(target_endian = "big")]
        return x.swap_bytes();
    }

    /// Return the memory representation of this integer as a byte array in big-endian byte order.
    #[inline]
    pub const fn to_be_bytes(self) -> B256 {
        self.to_be().to_ne_bytes()
    }

    /// Return the memory representation of this integer as a byte array in little-endian byte
    /// order.
    #[inline]
    pub const fn to_le_bytes(self) -> B256 {
        self.to_le().to_ne_bytes()
    }

    /// Return the memory representation of this integer as a byte array in native byte order.
    #[inline]
    pub const fn to_ne_bytes(self) -> B256 {
        self.0
    }

    /// Converts `self` to big endian from the target's endianness.
    #[inline]
    pub const fn to_be(self) -> Self {
        #[cfg(target_endian = "little")]
        return self.swap_bytes();
        #[cfg(target_endian = "big")]
        return self;
    }

    /// Converts `self` to little endian from the target's endianness.
    #[inline]
    pub const fn to_le(self) -> Self {
        #[cfg(target_endian = "little")]
        return self;
        #[cfg(target_endian = "big")]
        return self.swap_bytes();
    }

    /// Reverses the byte order of the integer.
    #[inline]
    pub const fn swap_bytes(mut self) -> Self {
        self.0.0.reverse();
        self
    }

    /// Casts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[cfg(target_endian = "little")]
    #[inline]
    pub const fn as_u256(&self) -> &U256 {
        unsafe { &*(self as *const Self as *const U256) }
    }

    /// Casts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[cfg(target_endian = "little")]
    #[inline]
    pub const fn as_u256_mut(&mut self) -> &mut U256 {
        unsafe { &mut *(self as *mut Self as *mut U256) }
    }

    /// Converts this value to a [`U256`]. This is a simple copy on little-endian systems.
    #[inline]
    pub const fn to_u256(&self) -> U256 {
        #[cfg(target_endian = "little")]
        return *self.as_u256();
        #[cfg(target_endian = "big")]
        return U256::from_be_bytes(self.0.0);
    }

    /// Converts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[inline]
    pub const fn into_u256(self) -> U256 {
        #[cfg(target_endian = "little")]
        return unsafe { core::mem::transmute::<Self, U256>(self) };
        #[cfg(target_endian = "big")]
        return U256::from_be_bytes(self.0.0);
    }

    /// Converts this value to an [`Address`].
    #[inline]
    pub fn to_address(self) -> Address {
        Address::from_word(self.to_be_bytes())
    }
}

// Macro re-exports.
// Not public API.
#[doc(hidden)]
pub mod private {
    pub use revm_interpreter;
    pub use revm_primitives;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let mut word = EvmWord::ZERO;
        assert_eq!(usize::try_from(word), Ok(0));
        assert_eq!(usize::try_from(&word), Ok(0));
        assert_eq!(usize::try_from(&mut word), Ok(0));
    }

    extern_revmc! {
        #[link_name = "__test_fn"]
        fn test_fn;
    }

    #[unsafe(no_mangle)]
    extern "C" fn __test_fn(
        _ecx: NonNull<EvmContext<'_>>,
        _stack: NonNull<EvmStack>,
        _stack_len: NonNull<usize>,
    ) -> InstructionResult {
        InstructionResult::Stop
    }

    #[test]
    fn extern_macro() {
        let f1 = EvmCompilerFn::new(test_fn).0;
        let f2 = EvmCompilerFn::new(__test_fn).0;
        assert!(core::ptr::fn_addr_eq(f1, f2), "{f1:?} != {f2:?}");
    }
}
