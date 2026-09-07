//! Safe wrappers for LLVM ORC and LLJIT, which are not yet implemented in [`inkwell`].
//!
//! Header files implemented:
//! - `LLJIT.h`
//! - `LLJITUtils.h` (experimental, not available in `llvm-sys`)
//! - `Orc.h`
//! - `OrcEE.h` (experimental)
//!
//! Note that lifetimes are implicit here, I just didn't bother to add them.

#![allow(missing_debug_implementations)]
#![allow(clippy::new_without_default)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]

use std::{
    ffi::{CStr, CString, c_char, c_void},
    fmt,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    panic::AssertUnwindSafe,
    ptr::{self, NonNull},
};

use inkwell::{
    context::Context,
    llvm_sys::{
        core::{
            LLVMContextCreate, LLVMCreateMemoryBufferWithMemoryRangeCopy,
            LLVMModuleCreateWithNameInContext,
        },
        error::*,
        orc2::{lljit::*, *},
        prelude::*,
    },
    module::Module,
    support::LLVMString,
    targets::TargetMachine,
};

use crate::llvm_string;

unsafe extern "C" {
    fn LLVMOrcCreateNewThreadSafeContextFromLLVMContext(
        Ctx: LLVMContextRef,
    ) -> LLVMOrcThreadSafeContextRef;
}

/// A thread-safe LLVM context.
///
/// Must use a lock to access the context in multi-threaded scenarios.
///
/// See [the ORCv2 docs](https://releases.llvm.org/17.0.1/docs/ORCv2.html).
pub struct ThreadSafeContext {
    ctx: LLVMOrcThreadSafeContextRef,
}

impl fmt::Debug for ThreadSafeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadSafeContext").finish_non_exhaustive()
    }
}

impl ThreadSafeContext {
    /// Creates a new thread-safe context.
    pub fn new() -> Self {
        unsafe { Self::from_inner(LLVMOrcCreateNewThreadSafeContext()) }
    }

    /// Creates a thread-safe context from an existing LLVM context.
    pub fn from_context(ctx: Context) -> Self {
        let ctx = mem::ManuallyDrop::new(ctx);
        unsafe { Self::from_inner(LLVMOrcCreateNewThreadSafeContextFromLLVMContext(ctx.raw())) }
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ctx: LLVMOrcThreadSafeContextRef) -> Self {
        Self { ctx }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcThreadSafeContextRef {
        self.ctx
    }

    /// Create a ThreadSafeModule wrapper around the given LLVM module.
    pub fn create_module<'ctx>(&'ctx self, module: Module<'ctx>) -> ThreadSafeModule {
        ThreadSafeModule::create_in_context(module, self)
    }
}

impl Drop for ThreadSafeContext {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeThreadSafeContext(self.ctx) };
    }
}

/// A thread-safe module.
///
/// See [the ORCv2 docs](https://releases.llvm.org/17.0.1/docs/ORCv2.html).
// NOTE: A lifetime is not needed according to `LLVMOrcCreateNewThreadSafeContext`
// > Ownership of the underlying ThreadSafeContext data is shared: Clients can and should dispose of
// > their ThreadSafeContext as soon as they no longer need to refer to it directly. Other
// > references (e.g. from ThreadSafeModules) will keep the data alive as long as it is needed.
pub struct ThreadSafeModule {
    ptr: LLVMOrcThreadSafeModuleRef,
}

impl ThreadSafeModule {
    /// Creates a new module with the given name in a new context.
    ///
    /// Use [`Self::with_module`] to access and modify the module.
    pub fn create(name: &str) -> Self {
        let name_cstr = CString::new(name).unwrap();
        unsafe {
            let ctx = LLVMContextCreate();
            let module = LLVMModuleCreateWithNameInContext(name_cstr.as_ptr(), ctx);
            let tsc = LLVMOrcCreateNewThreadSafeContextFromLLVMContext(ctx);
            let ptr = LLVMOrcCreateNewThreadSafeModule(module, tsc);
            LLVMOrcDisposeThreadSafeContext(tsc);
            Self { ptr }
        }
    }

    /// Create a ThreadSafeModule wrapper around the given LLVM module.
    pub fn create_in_context<'ctx>(module: Module<'ctx>, ctx: &'ctx ThreadSafeContext) -> Self {
        let module = mem::ManuallyDrop::new(module);
        let ptr = unsafe { LLVMOrcCreateNewThreadSafeModule(module.as_mut_ptr(), ctx.as_inner()) };
        Self { ptr }
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcThreadSafeModuleRef) -> Self {
        Self { ptr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcThreadSafeModuleRef {
        self.ptr
    }

    /// Runs the given closure with the module.
    ///
    /// This implicitly locks the associated context.
    pub fn with_module<'tsm>(
        &'tsm self,
        mut f: impl FnMut(&Module<'tsm>) -> Result<(), String>,
    ) -> Result<(), LLVMString> {
        extern "C" fn shim(ctx: *mut c_void, m: LLVMModuleRef) -> LLVMErrorRef {
            let f = ctx.cast::<&mut dyn FnMut(&Module<'_>) -> Result<(), String>>();
            let m = mem::ManuallyDrop::new(unsafe { Module::new(m) });
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe { (*f)(&m) }));
            cvt_cb_res(res)
        }

        let mut f = &mut f as &mut dyn FnMut(&Module<'tsm>) -> Result<(), String>;
        let ctx = &mut f as *mut _ as *mut c_void;
        cvt(unsafe { LLVMOrcThreadSafeModuleWithModuleDo(self.as_inner(), shim, ctx) })
    }
}

impl Drop for ThreadSafeModule {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeThreadSafeModule(self.ptr) };
    }
}

/// A symbol string pool reference.
#[repr(C)]
pub struct SymbolStringPoolRef {
    ptr: LLVMOrcSymbolStringPoolRef,
}

impl SymbolStringPoolRef {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcSymbolStringPoolRef) -> Self {
        Self { ptr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcSymbolStringPoolRef {
        self.ptr
    }

    /// Clear all unreferenced symbol string pool entries.
    ///
    /// This can be called at any time to release unused entries in the
    /// ExecutionSession's string pool. Since it locks the pool (preventing
    /// interning of any new strings) it is recommended that it only be called
    /// infrequently, ideally when the caller has reason to believe that some
    /// entries will have become unreferenced, e.g. after removing a module or
    /// closing a JITDylib.
    pub fn clear_dead_entries(&self) {
        unsafe { LLVMOrcSymbolStringPoolClearDeadEntries(self.as_inner()) };
    }
}

/// A reference-counted unique string interned in a `SymbolStringPool`.
#[derive(PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SymbolStringPoolEntry {
    ptr: NonNull<LLVMOrcOpaqueSymbolStringPoolEntry>,
}

impl Clone for SymbolStringPoolEntry {
    fn clone(&self) -> Self {
        unsafe { LLVMOrcRetainSymbolStringPoolEntry(self.as_inner()) };
        Self { ..*self }
    }
}

impl fmt::Debug for SymbolStringPoolEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_cstr().fmt(f)
    }
}

impl std::ops::Deref for SymbolStringPoolEntry {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        self.as_cstr()
    }
}

impl SymbolStringPoolEntry {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcSymbolStringPoolEntryRef) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr })
    }

    /// Wraps a raw pointer. Must not be null.
    pub unsafe fn from_inner_unchecked(ptr: LLVMOrcSymbolStringPoolEntryRef) -> Self {
        Self { ptr: NonNull::new_unchecked(ptr) }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcSymbolStringPoolEntryRef {
        self.ptr.as_ptr()
    }

    /// Convert to a C string.
    pub fn as_cstr(&self) -> &CStr {
        unsafe { CStr::from_ptr(LLVMOrcSymbolStringPoolEntryStr(self.as_inner())) }
    }
}

impl Drop for SymbolStringPoolEntry {
    fn drop(&mut self) {
        unsafe { LLVMOrcReleaseSymbolStringPoolEntry(self.as_inner()) }
    }
}

/// An evaluated symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct EvaluatedSymbol {
    /// The address of the symbol.
    pub address: u64,
    /// The flags of the symbol.
    pub flags: SymbolFlags,
}

impl EvaluatedSymbol {
    /// Create a new EvaluatedSymbol from the given address and flags.
    pub fn new(address: u64, flags: SymbolFlags) -> Self {
        Self { address, flags }
    }

    /// Create a new EvaluatedSymbol from the given flags.
    pub fn from_flags(flags: SymbolFlags) -> Self {
        Self { address: 0, flags }
    }

    /// Create a new EvaluatedSymbol from the given address.
    pub fn from_address(address: usize) -> Self {
        Self { address: address as u64, flags: SymbolFlags::none() }
    }
}

/// Symbol flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SymbolFlags {
    /// The generic flags.
    pub generic: u8,
    /// The target flags.
    pub target: u8,
}

impl Default for SymbolFlags {
    fn default() -> Self {
        Self::none()
    }
}

impl SymbolFlags {
    /// Create a new, empty SymbolFlags.
    pub fn none() -> Self {
        Self { generic: LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsNone as u8, target: 0 }
    }

    /// Set the `Exported` flag.
    pub fn set_exported(&mut self) {
        self.generic |= LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsExported as u8;
    }

    /// Set the `Exported` flag.
    pub fn exported(mut self) -> Self {
        self.set_exported();
        self
    }

    /// Returns `true` if the `Exported` flag is set.
    pub fn is_exported(&self) -> bool {
        (self.generic & LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsExported as u8) != 0
    }

    /// Set the `Weak` flag.
    pub fn set_weak(&mut self) {
        self.generic |= LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsWeak as u8;
    }

    /// Set the `Weak` flag.
    pub fn weak(mut self) -> Self {
        self.set_weak();
        self
    }

    /// Returns `true` if the `Weak` flag is set.
    pub fn is_weak(&self) -> bool {
        (self.generic & LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsWeak as u8) != 0
    }

    /// Set the `Callable` flag.
    pub fn set_callable(&mut self) {
        self.generic |= LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsCallable as u8;
    }

    /// Set the `Callable` flag.
    pub fn callable(mut self) -> Self {
        self.set_callable();
        self
    }

    /// Returns `true` if the `Callable` flag is set.
    pub fn is_callable(&self) -> bool {
        (self.generic & LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsCallable as u8) != 0
    }

    /// Set the `MaterializationSideEffectsOnly` flag.
    pub fn set_materialization_side_effects_only(&mut self) {
        self.generic |=
            LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsMaterializationSideEffectsOnly
                as u8;
    }

    /// Set the `MaterializationSideEffectsOnly` flag.
    pub fn materialization_side_effects_only(mut self) -> Self {
        self.set_materialization_side_effects_only();
        self
    }

    /// Returns `true` if the `MaterializationSideEffectsOnly` flag is set.
    pub fn is_materialization_side_effects_only(&self) -> bool {
        (self.generic
            & LLVMJITSymbolGenericFlags::LLVMJITSymbolGenericFlagsMaterializationSideEffectsOnly
                as u8)
            != 0
    }

    /// Add a generic flag.
    pub fn set_generic(&mut self, flag: LLVMJITSymbolGenericFlags) {
        self.generic |= flag as u8;
    }

    /// Add a generic flag.
    pub fn with_generic(mut self, flag: LLVMJITSymbolGenericFlags) -> Self {
        self.set_generic(flag);
        self
    }

    /// Add a target flag.
    pub fn set_target(&mut self, flag: LLVMJITSymbolTargetFlags) {
        self.target |= flag;
    }

    /// Add a target flag.
    pub fn with_target(mut self, flag: LLVMJITSymbolTargetFlags) -> Self {
        self.set_target(flag);
        self
    }
}

/// A pair of a symbol name and flags.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SymbolFlagsMapPair {
    /// The symbol name.
    pub name: SymbolStringPoolEntry,
    /// The symbol flags.
    pub flags: SymbolFlags,
}

impl SymbolFlagsMapPair {
    /// Create a new pair.
    pub fn new(name: SymbolStringPoolEntry, flags: SymbolFlags) -> Self {
        Self { name, flags }
    }
}

/// A pair of a symbol name and an evaluated symbol.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct SymbolMapPair {
    /// The symbol name.
    pub name: SymbolStringPoolEntry,
    /// The evaluated symbol.
    pub evaluated_symbol: EvaluatedSymbol,
}

impl SymbolMapPair {
    /// Create a new pair.
    pub fn new(name: SymbolStringPoolEntry, evaluated_symbol: EvaluatedSymbol) -> Self {
        Self { name, evaluated_symbol }
    }
}

/// An owned list of symbol flags map pairs.
///
/// Returned by [`MaterializationResponsibilityRef::get_symbols`].
pub struct SymbolFlagsMapPairs<'a>(&'a [SymbolFlagsMapPair]);

impl<'a> SymbolFlagsMapPairs<'a> {
    /// Returns the slice of pairs.
    pub fn as_slice(&self) -> &'a [SymbolFlagsMapPair] {
        self.0
    }
}

impl std::ops::Deref for SymbolFlagsMapPairs<'_> {
    type Target = [SymbolFlagsMapPair];

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl Drop for SymbolFlagsMapPairs<'_> {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeCSymbolFlagsMap(self.0.as_ptr().cast_mut().cast()) };
    }
}

/// A materialization unit.
pub struct MaterializationUnit {
    mu: LLVMOrcMaterializationUnitRef,
}

impl fmt::Debug for MaterializationUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaterializationUnit").finish_non_exhaustive()
    }
}

impl MaterializationUnit {
    /// Create a custom MaterializationUnit.
    pub fn new_custom(
        name: &CStr,
        syms: Vec<SymbolFlagsMapPair>,
        init_sym: Option<SymbolStringPoolEntry>,
        mu: Box<dyn CustomMaterializationUnit>,
    ) -> Self {
        extern "C" fn materialize(ctx: *mut c_void, mr: LLVMOrcMaterializationResponsibilityRef) {
            // Ownership of the Ctx and MR arguments passes to the callback which must adhere to the
            // LLVMOrcMaterializationResponsibilityRef contract (see comment for that type).
            //
            // If this callback is called then the LLVMOrcMaterializationUnitDestroy callback will
            // NOT be called.
            let ctx = unsafe { Box::from_raw(ctx.cast::<Box<dyn CustomMaterializationUnit>>()) };
            let mr = unsafe { MaterializationResponsibility::from_inner(mr) };
            let res = std::panic::catch_unwind(AssertUnwindSafe(move || ctx.materialize(mr)));
            if let Err(e) = res {
                error!(msg=?panic_payload(&e), "materialize callback panicked");
            }
        }

        extern "C" fn discard(
            ctx: *mut c_void,
            jd: LLVMOrcJITDylibRef,
            symbol: LLVMOrcSymbolStringPoolEntryRef,
        ) {
            // Ownership of JD and Symbol remain with the caller:
            // these arguments should not be disposed of or released.
            let ctx = unsafe { &mut **ctx.cast::<Box<dyn CustomMaterializationUnit>>() };
            let jd = unsafe { JITDylibRef::from_inner_unchecked(jd) };
            let symbol = mem::ManuallyDrop::new(unsafe {
                SymbolStringPoolEntry::from_inner_unchecked(symbol)
            });
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| ctx.discard(jd, &symbol)));
            if let Err(e) = res {
                error!(msg=?panic_payload(&e), "discard callback panicked");
            }
        }

        extern "C" fn destroy(ctx: *mut c_void) {
            // If a custom MaterializationUnit is destroyed before its Materialize function is
            // called then this function will be called to provide an opportunity for the underlying
            // program representation to be destroyed.
            let ctx = unsafe { Box::from_raw(ctx.cast::<Box<dyn CustomMaterializationUnit>>()) };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| drop(ctx)));
            if let Err(e) = res {
                error!(msg=?panic_payload(&e), "destroy callback panicked");
            }
        }

        let ctx = Box::into_raw(Box::new(mu)).cast();
        let init_sym = if let Some(init_sym) = init_sym {
            mem::ManuallyDrop::new(init_sym).as_inner()
        } else {
            ptr::null_mut()
        };
        let syms = ManuallyDropElements::new(syms);
        unsafe {
            Self::new_custom_raw(
                name,
                ctx,
                syms.as_ptr().cast_mut().cast(),
                syms.len(),
                init_sym,
                materialize,
                discard,
                destroy,
            )
        }
    }

    /// Create a custom MaterializationUnit.
    ///
    /// See [`Self::new_custom`].
    pub unsafe fn new_custom_raw(
        name: &CStr,
        ctx: *mut c_void,
        syms: LLVMOrcCSymbolFlagsMapPairs,
        num_syms: usize,
        init_sym: LLVMOrcSymbolStringPoolEntryRef,
        materialize: LLVMOrcMaterializationUnitMaterializeFunction,
        discard: LLVMOrcMaterializationUnitDiscardFunction,
        destroy: LLVMOrcMaterializationUnitDestroyFunction,
    ) -> Self {
        Self::from_inner(LLVMOrcCreateCustomMaterializationUnit(
            name.as_ptr(),
            ctx,
            syms,
            num_syms,
            init_sym,
            materialize,
            discard,
            destroy,
        ))
    }

    /// Create a MaterializationUnit to define the given symbols as pointing to the corresponding
    /// raw addresses.
    pub fn absolute_symbols(syms: Vec<SymbolMapPair>) -> Self {
        let syms = ManuallyDropElements::new(syms);
        unsafe { Self::absolute_symbols_raw(syms.as_ptr().cast_mut().cast(), syms.len()) }
    }

    /// Create a MaterializationUnit to define the given symbols as pointing to the corresponding
    /// raw addresses.
    ///
    /// See [`Self::absolute_symbols`].
    pub unsafe fn absolute_symbols_raw(syms: LLVMOrcCSymbolMapPairs, len: usize) -> Self {
        unsafe { Self::from_inner(LLVMOrcAbsoluteSymbols(syms, len)) }
    }

    // TODO: fn lazy_reexports

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(mu: LLVMOrcMaterializationUnitRef) -> Self {
        Self { mu }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcMaterializationUnitRef {
        self.mu
    }
}

impl Drop for MaterializationUnit {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeMaterializationUnit(self.as_inner()) };
    }
}

/// A custom materialization unit.
///
/// Use with [`MaterializationUnit::new_custom`].
pub trait CustomMaterializationUnit {
    /// Materialize callback.
    fn materialize(self: Box<Self>, mr: MaterializationResponsibility);

    /// Discard callback.
    fn discard(&mut self, jd: JITDylibRef, symbol: &SymbolStringPoolEntry);

    // fn destroy is Drop
}

/// An owned materialization responsibility.
pub struct MaterializationResponsibility {
    mr: LLVMOrcMaterializationResponsibilityRef,
}

impl MaterializationResponsibility {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(mr: LLVMOrcMaterializationResponsibilityRef) -> Self {
        Self { mr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcMaterializationResponsibilityRef {
        self.mr
    }

    /// Returns a reference to the MaterializationResponsibility.
    #[inline]
    pub fn as_ref(&self) -> MaterializationResponsibilityRef<'_> {
        unsafe { MaterializationResponsibilityRef::from_inner(self.as_inner()) }
    }
}

impl Drop for MaterializationResponsibility {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeMaterializationResponsibility(self.as_inner()) };
    }
}

/// A reference to a materialization responsibility.
pub struct MaterializationResponsibilityRef<'mr> {
    mr: LLVMOrcMaterializationResponsibilityRef,
    _marker: PhantomData<&'mr ()>,
}

impl<'mr> MaterializationResponsibilityRef<'mr> {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(mr: LLVMOrcMaterializationResponsibilityRef) -> Self {
        Self { mr, _marker: PhantomData }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcMaterializationResponsibilityRef {
        self.mr
    }

    /// Returns the target JITDylib that these symbols are being materialized into.
    pub fn get_target_dylib(&self) -> JITDylibRef {
        unsafe {
            JITDylibRef::from_inner_unchecked(LLVMOrcMaterializationResponsibilityGetTargetDylib(
                self.as_inner(),
            ))
        }
    }

    /// Returns the ExecutionSession for this MaterializationResponsibility.
    pub fn get_execution_session(&self) -> ExecutionSessionRef<'mr> {
        unsafe {
            ExecutionSessionRef::from_inner(
                LLVMOrcMaterializationResponsibilityGetExecutionSession(self.as_inner()),
            )
        }
    }

    /// Returns the symbol flags map for this responsibility instance.
    pub fn get_symbols(&self) -> SymbolFlagsMapPairs<'mr> {
        /*
         * The length of the array is returned in NumPairs and the caller is responsible
         * for the returned memory and needs to call LLVMOrcDisposeCSymbolFlagsMap.
         *
         * To use the returned symbols beyond the lifetime of the
         * MaterializationResponsibility requires the caller to retain the symbols
         * explicitly.
         */
        let mut len = MaybeUninit::uninit();
        let data = unsafe {
            LLVMOrcMaterializationResponsibilityGetSymbols(self.as_inner(), len.as_mut_ptr())
        };
        SymbolFlagsMapPairs(unsafe { std::slice::from_raw_parts(data.cast(), len.assume_init()) })
    }

    /// Returns the initialization pseudo-symbol, if any.
    ///
    /// This symbol will also be present in the SymbolFlagsMap for this
    /// MaterializationResponsibility object.
    pub fn get_initializer_symbol(&self) -> Option<SymbolStringPoolEntry> {
        let ptr =
            unsafe { LLVMOrcMaterializationResponsibilityGetInitializerSymbol(self.as_inner()) };
        unsafe { SymbolStringPoolEntry::from_inner(ptr) }
    }

    /// Returns the names of any symbols covered by this MaterializationResponsibility object that
    /// have queries pending.
    ///
    /// This information can be used to return responsibility for unrequested symbols back to the
    /// JITDylib via the delegate method.
    pub fn get_requested_symbols(&self) -> &[SymbolStringPoolEntry] {
        let mut len = MaybeUninit::uninit();
        let ptr = unsafe {
            LLVMOrcMaterializationResponsibilityGetRequestedSymbols(
                self.as_inner(),
                len.as_mut_ptr(),
            )
        };
        unsafe { std::slice::from_raw_parts(ptr.cast(), len.assume_init()) }
    }

    /// Notifies the target JITDylib that the given symbols have been resolved.
    pub fn notify_resolved(&self, syms: &[SymbolFlagsMapPair]) -> Result<(), LLVMString> {
        cvt(unsafe {
            LLVMOrcMaterializationResponsibilityNotifyResolved(
                self.as_inner(),
                syms.as_ptr().cast_mut().cast(),
                syms.len(),
            )
        })
    }

    /// Notifies the target JITDylib (and any pending queries on that JITDylib)
    /// that all symbols covered by this MaterializationResponsibility instance
    /// have been emitted.
    ///
    /// `symbol_dep_groups` specifies the symbol dependency groups for this materialization unit.
    /// Pass an empty slice if there are no dependencies.
    pub fn notify_emitted(
        &self,
        symbol_dep_groups: &mut [LLVMOrcCSymbolDependenceGroup],
    ) -> Result<(), LLVMString> {
        cvt(unsafe {
            LLVMOrcMaterializationResponsibilityNotifyEmitted(
                self.as_inner(),
                symbol_dep_groups.as_mut_ptr(),
                symbol_dep_groups.len(),
            )
        })
    }

    /// Notify all not-yet-emitted covered by this MaterializationResponsibility instance that an
    /// error has occurred.
    ///
    /// This will remove all symbols covered by this MaterializationResponsibility from the target
    /// JITDylib, and send an error to any queries waiting on these symbols.
    pub fn fail_materialization(&self) {
        unsafe { LLVMOrcMaterializationResponsibilityFailMaterialization(self.as_inner()) };
    }

    /// Transfers responsibility to the given MaterializationUnit for all symbols defined by that
    /// MaterializationUnit.
    ///
    /// This allows materializers to break up work based on run-time information (e.g.
    /// by introspecting which symbols have actually been looked up and
    /// materializing only those).
    pub fn replace(&self, mu: MaterializationUnit) -> Result<(), LLVMString> {
        let mu = mem::ManuallyDrop::new(mu);
        cvt(unsafe { LLVMOrcMaterializationResponsibilityReplace(self.as_inner(), mu.as_inner()) })
    }

    /// Delegates responsibility for the given symbols to the returned
    /// materialization responsibility. Useful for breaking up work between
    /// threads, or different kinds of materialization processes.
    ///
    /// The caller retains responsibility of the passed
    /// MaterializationResponsibility.
    pub fn delegate(&self, syms: &[SymbolStringPoolEntry]) -> Result<Self, LLVMString> {
        let mut res = MaybeUninit::uninit();
        cvt(unsafe {
            LLVMOrcMaterializationResponsibilityDelegate(
                self.as_inner(),
                syms.as_ptr().cast_mut().cast(),
                syms.len(),
                res.as_mut_ptr(),
            )
        })?;
        Ok(unsafe { Self::from_inner(res.assume_init()) })
    }
}

/// A resource tracker.
///
/// ResourceTrackers allow you to remove code.
pub struct ResourceTracker {
    rt: LLVMOrcResourceTrackerRef,
}

impl fmt::Debug for ResourceTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceTracker").finish_non_exhaustive()
    }
}

impl ResourceTracker {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(rt: LLVMOrcResourceTrackerRef) -> Self {
        Self { rt }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcResourceTrackerRef {
        self.rt
    }

    /// Remove all resources associated with this tracker.
    pub fn remove(&self) -> Result<(), LLVMString> {
        cvt(unsafe { LLVMOrcResourceTrackerRemove(self.as_inner()) })
    }

    /// Transfers tracking of all resources associated with this resource tracker to the given
    /// resource tracker.
    pub fn transfer_to(&self, rt: &Self) {
        unsafe { LLVMOrcResourceTrackerTransferTo(self.as_inner(), rt.as_inner()) };
    }
}

impl Drop for ResourceTracker {
    fn drop(&mut self) {
        unsafe { LLVMOrcReleaseResourceTracker(self.as_inner()) };
    }
}

/// A JIT execution session reference.
///
/// Returned by [`LLJIT::get_execution_session`] and
/// [`MaterializationResponsibilityRef::get_execution_session`].
///
/// ExecutionSession represents the JIT'd program and provides context for the JIT: It contains the
/// JITDylibs, error reporting mechanisms, and dispatches the materializers.
///
/// See [the ORCv2 docs](https://releases.llvm.org/17.0.1/docs/ORCv2.html).
pub struct ExecutionSessionRef<'ee> {
    es: LLVMOrcExecutionSessionRef,
    _marker: PhantomData<&'ee ()>,
}

impl ExecutionSessionRef<'_> {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(es: LLVMOrcExecutionSessionRef) -> Self {
        Self { es, _marker: PhantomData }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcExecutionSessionRef {
        self.es
    }

    /// Returns the SymbolStringPool for this ExecutionSession.
    pub fn get_symbol_string_pool(&self) -> SymbolStringPoolRef {
        unsafe {
            SymbolStringPoolRef::from_inner(LLVMOrcExecutionSessionGetSymbolStringPool(
                self.as_inner(),
            ))
        }
    }

    /// Intern a string in the ExecutionSession's SymbolStringPool and return a reference to it.
    pub fn intern(&self, name: &CStr) -> SymbolStringPoolEntry {
        unsafe {
            SymbolStringPoolEntry::from_inner_unchecked(LLVMOrcExecutionSessionIntern(
                self.as_inner(),
                name.as_ptr(),
            ))
        }
    }

    /// Returns the JITDylib with the given name, if any.
    pub fn get_dylib_by_name(&self, name: &CStr) -> Option<JITDylibRef> {
        unsafe {
            let dylib = LLVMOrcExecutionSessionGetJITDylibByName(self.as_inner(), name.as_ptr());
            JITDylibRef::from_inner(dylib)
        }
    }

    /// Create a "bare" JITDylib.
    ///
    /// The client is responsible for ensuring that the JITDylib's name is unique.
    ///
    /// This call does not install any library code or symbols into the newly created JITDylib. The
    /// client is responsible for all configuration.
    pub fn create_bare_jit_dylib(&self, name: &CStr) -> JITDylibRef {
        debug_assert!(self.get_dylib_by_name(name).is_none());
        unsafe {
            JITDylibRef::from_inner_unchecked(LLVMOrcExecutionSessionCreateBareJITDylib(
                self.as_inner(),
                name.as_ptr(),
            ))
        }
    }

    /// Create a JITDylib.
    ///
    /// The client is responsible for ensuring that the JITDylib's name is unique.
    ///
    /// If a Platform is attached to the ExecutionSession then Platform::setupJITDylib will be
    /// called to install standard platform symbols (e.g. standard library interposes). If no
    /// Platform is installed then this call is equivalent to [Self::create_bare_jit_dylib] and will
    /// always return success.
    pub fn create_jit_dylib(&self, name: &CStr) -> Result<JITDylibRef, LLVMString> {
        debug_assert!(self.get_dylib_by_name(name).is_none());
        let mut res = MaybeUninit::uninit();
        cvt(unsafe {
            LLVMOrcExecutionSessionCreateJITDylib(self.as_inner(), res.as_mut_ptr(), name.as_ptr())
        })?;
        Ok(unsafe { JITDylibRef::from_inner_unchecked(res.assume_init()) })
    }

    /// Remove a JITDylib from the ExecutionSession, freeing all its resources once the last handle
    /// is dropped.
    pub fn remove_jit_dylib(&self, jd: JITDylibRef) -> Result<(), LLVMString> {
        cvt(unsafe {
            crate::cpp::revmc_llvm_execution_session_remove_jit_dylib(
                self.as_inner(),
                jd.as_inner(),
            )
        })
    }

    /// Sets the default error reporter to the ExecutionSession.
    ///
    /// Uses [`tracing::error!`] to log the error message.
    pub fn set_default_error_reporter(&self) {
        self.set_error_reporter(|msg| error!(msg = %msg.to_string_lossy(), "LLVM error"))
    }

    /// Attach a custom error reporter function to the ExecutionSession.
    pub fn set_error_reporter(&self, f: fn(&CStr)) {
        extern "C" fn shim(ctx: *mut c_void, err: LLVMErrorRef) {
            let f: fn(&CStr) = unsafe { mem::transmute(ctx) };
            let Err(e) = cvt(err) else { return };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| f(&e)));
            if let Err(e) = res {
                error!(msg=?panic_payload(&e), "error reporter closure panicked");
            }
        }

        let ctx = f as *mut c_void;
        unsafe { LLVMOrcExecutionSessionSetErrorReporter(self.as_inner(), shim, ctx) };
    }
}

/// A JIT dynamic library reference.
///
/// JITDylibs provide the symbol tables.
#[derive(Clone, Copy)]
pub struct JITDylibRef {
    dylib: NonNull<LLVMOrcOpaqueJITDylib>,
}

impl JITDylibRef {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(dylib: LLVMOrcJITDylibRef) -> Option<Self> {
        NonNull::new(dylib).map(|dylib| Self { dylib })
    }

    /// Wraps a raw pointer. Must not be null.
    pub unsafe fn from_inner_unchecked(dylib: LLVMOrcJITDylibRef) -> Self {
        Self { dylib: NonNull::new_unchecked(dylib) }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(self) -> LLVMOrcJITDylibRef {
        self.dylib.as_ptr()
    }

    /// Return a reference to a newly created resource tracker associated with JD.
    pub fn create_resource_tracker(self) -> ResourceTracker {
        unsafe {
            ResourceTracker::from_inner(LLVMOrcJITDylibCreateResourceTracker(self.as_inner()))
        }
    }

    /// Return a reference to the default resource tracker for the given JITDylib.
    pub fn get_default_resource_tracker(self) -> ResourceTracker {
        unsafe {
            ResourceTracker::from_inner(LLVMOrcJITDylibGetDefaultResourceTracker(self.as_inner()))
        }
    }

    /// Add the given MaterializationUnit to the given JITDylib.
    pub fn define(self, mu: MaterializationUnit) -> Result<(), (LLVMString, MaterializationUnit)> {
        // If this operation succeeds then JITDylib JD will take ownership of MU.
        // If the operation fails then ownership remains with the caller who should call
        // LLVMOrcDisposeMaterializationUnit to destroy it.
        let mu = mem::ManuallyDrop::new(mu);
        cvt(unsafe { LLVMOrcJITDylibDefine(self.as_inner(), mu.as_inner()) })
            .map_err(|e| (e, mem::ManuallyDrop::into_inner(mu)))
    }

    /// Add another JITDylib to this JITDylib's link order.
    ///
    /// Symbols not found in this JITDylib will be searched for in `other`.
    pub fn add_to_link_order(self, other: Self) {
        unsafe {
            crate::cpp::revmc_llvm_jit_dylib_add_to_link_order(self.as_inner(), other.as_inner())
        };
    }

    /// Calls remove on all trackers associated with this JITDylib.
    pub fn clear(self) -> Result<(), LLVMString> {
        cvt(unsafe { LLVMOrcJITDylibClear(self.as_inner()) })
    }

    /// Add a DefinitionGenerator to the given JITDylib.
    pub fn add_generator(self, dg: DefinitionGenerator) {
        // The JITDylib will take ownership of the given generator:
        // the client is no longer responsible for managing its memory.
        let dg = mem::ManuallyDrop::new(dg);
        unsafe { LLVMOrcJITDylibAddGenerator(self.as_inner(), dg.as_inner()) };
    }
}

/// Definition generator.
pub struct DefinitionGenerator {
    dg: LLVMOrcDefinitionGeneratorRef,
}

impl DefinitionGenerator {
    /// Creates a generator that resolves symbols from the current process.
    pub fn for_current_process(global_prefix: c_char) -> Result<Self, LLVMString> {
        let mut generator = MaybeUninit::uninit();
        cvt(unsafe {
            LLVMOrcCreateDynamicLibrarySearchGeneratorForProcess(
                generator.as_mut_ptr(),
                global_prefix,
                None,
                ptr::null_mut(),
            )
        })?;
        Ok(unsafe { Self::from_inner(generator.assume_init()) })
    }

    /// Creates a new custom DefinitionGenerator.
    pub fn new_custom(generator: Box<dyn CustomDefinitionGenerator>) -> Self {
        extern "C" fn try_to_generate(
            generator_obj: LLVMOrcDefinitionGeneratorRef,
            ctx: *mut c_void,
            lookup_state: *mut LLVMOrcLookupStateRef,
            kind: LLVMOrcLookupKind,
            jd: LLVMOrcJITDylibRef,
            jd_lookup_flags: LLVMOrcJITDylibLookupFlags,
            lookup_set: LLVMOrcCLookupSet,
            lookup_set_size: usize,
        ) -> LLVMErrorRef {
            let generator = unsafe { &mut **ctx.cast::<Box<dyn CustomDefinitionGenerator>>() };
            let lookup_state = unsafe { &mut *lookup_state };
            let jd = unsafe { JITDylibRef::from_inner_unchecked(jd) };
            let lookup_set = unsafe { std::slice::from_raw_parts(lookup_set, lookup_set_size) };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
                generator.try_to_generate(
                    generator_obj,
                    lookup_state,
                    kind,
                    jd,
                    jd_lookup_flags,
                    lookup_set,
                )
            }));
            cvt_cb_res(res)
        }

        extern "C" fn dispose(ctx: *mut c_void) {
            let generator =
                unsafe { Box::from_raw(ctx.cast::<Box<dyn CustomDefinitionGenerator>>()) };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| drop(generator)));
            if let Err(e) = res {
                error!(msg=?panic_payload(&e), "dispose callback panicked");
            }
        }

        let ctx = Box::into_raw(Box::new(generator)).cast();
        unsafe { Self::new_custom_raw(try_to_generate, ctx, dispose) }
    }

    /// Creates a new custom DefinitionGenerator.
    ///
    /// See [`Self::new_custom`].
    pub unsafe fn new_custom_raw(
        f: LLVMOrcCAPIDefinitionGeneratorTryToGenerateFunction,
        ctx: *mut c_void,
        dispose: LLVMOrcDisposeCAPIDefinitionGeneratorFunction,
    ) -> Self {
        Self::from_inner(LLVMOrcCreateCustomCAPIDefinitionGenerator(f, ctx, dispose))
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(dg: LLVMOrcDefinitionGeneratorRef) -> Self {
        Self { dg }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcDefinitionGeneratorRef {
        self.dg
    }
}

impl Drop for DefinitionGenerator {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeDefinitionGenerator(self.as_inner()) };
    }
}

/// A custom definition generator.
pub trait CustomDefinitionGenerator {
    /// A custom generator function.
    ///
    /// This can be used to create a custom generator object using
    /// LLVMOrcCreateCustomCAPIDefinitionGenerator. The resulting object can be attached to a
    /// JITDylib, via LLVMOrcJITDylibAddGenerator, to receive callbacks when lookups fail to match
    /// existing definitions.
    ///
    /// GeneratorObj will contain the address of the custom generator object.
    ///
    /// Ctx will contain the context object passed to LLVMOrcCreateCustomCAPIDefinitionGenerator.
    ///
    /// LookupState will contain a pointer to an LLVMOrcLookupStateRef object. This can optionally
    /// be modified to make the definition generation process asynchronous: If the LookupStateRef
    /// value is copied, and the original LLVMOrcLookupStateRef set to null, the lookup will be
    /// suspended. Once the asynchronous definition process has been completed clients must call
    /// LLVMOrcLookupStateContinueLookup to continue the lookup (this should be done
    /// unconditionally, even if errors have occurred in the mean time, to free the lookup state
    /// memory and notify the query object of the failures). If LookupState is captured this
    /// function must return LLVMErrorSuccess.
    ///
    /// The Kind argument can be inspected to determine the lookup kind (e.g.
    /// as-if-during-static-link, or as-if-during-dlsym).
    ///
    /// The JD argument specifies which JITDylib the definitions should be generated into.
    ///
    /// The JDLookupFlags argument can be inspected to determine whether the original lookup
    /// included non-exported symbols.
    ///
    /// Finally, the LookupSet argument contains the set of symbols that could not be found in JD
    /// already (the set of generation candidates).
    fn try_to_generate(
        &mut self,
        generator_obj: LLVMOrcDefinitionGeneratorRef,
        lookup_state: &mut LLVMOrcLookupStateRef,
        kind: LLVMOrcLookupKind,
        jd: JITDylibRef,
        jd_lookup_flags: LLVMOrcJITDylibLookupFlags,
        lookup_set: &[LLVMOrcCLookupSetElement],
    ) -> Result<(), String>;
}

impl<T> CustomDefinitionGenerator for T
where
    T: FnMut(
        LLVMOrcDefinitionGeneratorRef,
        &mut LLVMOrcLookupStateRef,
        LLVMOrcLookupKind,
        JITDylibRef,
        LLVMOrcJITDylibLookupFlags,
        &[LLVMOrcCLookupSetElement],
    ) -> Result<(), String>,
{
    #[inline]
    fn try_to_generate(
        &mut self,
        generator_obj: LLVMOrcDefinitionGeneratorRef,
        lookup_state: &mut LLVMOrcLookupStateRef,
        kind: LLVMOrcLookupKind,
        jd: JITDylibRef,
        jd_lookup_flags: LLVMOrcJITDylibLookupFlags,
        lookup_set: &[LLVMOrcCLookupSetElement],
    ) -> Result<(), String> {
        self(generator_obj, lookup_state, kind, jd, jd_lookup_flags, lookup_set)
    }
}

/// [`LLVMOrcJITTargetMachineBuilderRef`], used in [`LLJITBuilder`].
pub struct JITTargetMachineBuilder {
    builder: LLVMOrcJITTargetMachineBuilderRef,
}

impl JITTargetMachineBuilder {
    /// Create a JITTargetMachineBuilder from the given TargetMachine template.
    pub fn new(tm: TargetMachine) -> Self {
        let tm = mem::ManuallyDrop::new(tm);
        unsafe {
            Self::from_inner(LLVMOrcJITTargetMachineBuilderCreateFromTargetMachine(tm.as_mut_ptr()))
        }
    }

    /// Create a JITTargetMachineBuilder by detecting the host.
    pub fn detect_host() -> Result<Self, LLVMString> {
        let mut res = MaybeUninit::uninit();
        cvt(unsafe { LLVMOrcJITTargetMachineBuilderDetectHost(res.as_mut_ptr()) })?;
        Ok(Self { builder: unsafe { res.assume_init() } })
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(builder: LLVMOrcJITTargetMachineBuilderRef) -> Self {
        Self { builder }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcJITTargetMachineBuilderRef {
        self.builder
    }

    /// Returns the target triple for the given JITTargetMachineBuilder as a string.
    pub fn get_target_triple(&self) -> LLVMString {
        unsafe { llvm_string(LLVMOrcJITTargetMachineBuilderGetTargetTriple(self.as_inner())) }
    }

    /// Sets the target triple for the given JITTargetMachineBuilder to the given string.
    pub fn set_target_triple(&self, triple: &CStr) {
        unsafe { LLVMOrcJITTargetMachineBuilderSetTargetTriple(self.as_inner(), triple.as_ptr()) }
    }
}

impl Drop for JITTargetMachineBuilder {
    fn drop(&mut self) {
        unsafe { LLVMOrcDisposeJITTargetMachineBuilder(self.builder) }
    }
}

/// Lazily-initialized [`LLJIT`] builder.
#[must_use]
pub struct LLJITBuilder {
    builder: lljit::LLVMOrcLLJITBuilderRef,
}

impl LLJITBuilder {
    /// Creates a new default LLJIT builder.
    pub fn new() -> Self {
        Self { builder: ptr::null_mut() }
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(builder: LLVMOrcLLJITBuilderRef) -> Self {
        Self { builder }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcLLJITBuilderRef {
        self.builder
    }

    fn as_inner_init(&mut self) -> LLVMOrcLLJITBuilderRef {
        if self.builder.is_null() {
            self.builder = unsafe { LLVMOrcCreateLLJITBuilder() };
        }
        self.builder
    }

    /// Set the target machine builder by creating it from the given template.
    pub fn set_target_machine(self, tm: TargetMachine) -> Self {
        self.set_target_machine_builder(JITTargetMachineBuilder::new(tm))
    }

    /// Set the target machine builder by detecting the host.
    pub fn set_target_machine_from_host(self) -> Result<Self, LLVMString> {
        JITTargetMachineBuilder::detect_host().map(|jtmb| self.set_target_machine_builder(jtmb))
    }

    /// Set the target machine builder.
    pub fn set_target_machine_builder(mut self, jtmb: JITTargetMachineBuilder) -> Self {
        let jtmb = mem::ManuallyDrop::new(jtmb);
        unsafe {
            LLVMOrcLLJITBuilderSetJITTargetMachineBuilder(self.as_inner_init(), jtmb.as_inner())
        };
        self
    }

    /*
    pub fn set_object_linking_layer_creator(self, mut f: &mut dyn FnMut(*const (), &CStr)) -> Self {
        extern "C" fn shim(
            ctx: *mut c_void,
            es: LLVMOrcExecutionSessionRef,
            triple: *const c_char,
        ) -> LLVMOrcObjectLayerRef {
            let f = ctx.cast::<&mut dyn FnMut(*const (), &CStr)>();
            let name = unsafe { CStr::from_ptr(triple) };
            unsafe { (*f)(es.cast(), name) };
            ptr::null_mut()
        }

        let ctx = &mut f as *mut _ as *mut c_void;
        unsafe { LLVMOrcLLJITBuilderSetObjectLinkingLayerCreator(self.as_inner_init(), shim, ctx) };
        self
    }
    */

    /// Use a thread-safe compiler without spawning background threads.
    ///
    /// Installs `ConcurrentIRCompiler` (fresh `TargetMachine` per compilation)
    /// so multiple caller threads can compile through the same LLJIT safely.
    /// The default `InPlaceTaskDispatcher` is preserved, meaning compilation
    /// runs inline on the thread that triggers it — no background threads.
    pub fn concurrent_compiler(mut self) -> Self {
        unsafe {
            crate::cpp::revmc_llvm_lljit_builder_set_concurrent_compiler(self.as_inner_init())
        };
        self
    }

    /// Builds the JIT.
    pub fn build(self) -> Result<LLJIT, LLVMString> {
        // This operation takes ownership of the Builder argument: clients should not
        // dispose of the builder after calling this function (even if the function
        // returns an error).
        let builder = mem::ManuallyDrop::new(self);
        let mut res = MaybeUninit::uninit();
        cvt(unsafe { LLVMOrcCreateLLJIT(res.as_mut_ptr(), builder.as_inner()) })?;
        Ok(unsafe { LLJIT::from_inner(res.assume_init()) })
    }
}

impl Drop for LLJITBuilder {
    fn drop(&mut self) {
        if !self.builder.is_null() {
            unsafe { LLVMOrcDisposeLLJITBuilder(self.builder) };
        }
    }
}

/// An ORC JIT.
///
/// Manages the memory of all JIT'd code and all modules that are transferred to it.
///
/// See [the ORCv2 docs](https://releases.llvm.org/17.0.1/docs/ORCv2.html).
pub struct LLJIT {
    jit: LLVMOrcLLJITRef,
}

impl fmt::Debug for LLJIT {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LLJIT").finish_non_exhaustive()
    }
}

impl LLJIT {
    /// Creates a new LLJIT builder.
    pub fn builder() -> LLJITBuilder {
        LLJITBuilder::new()
    }

    /// Creates a new ORC JIT, auto-detecting the host target.
    pub fn new() -> Result<Self, LLVMString> {
        LLJITBuilder::new().build()
    }

    /// Wraps a raw pointer.
    pub unsafe fn from_inner(jit: LLVMOrcLLJITRef) -> Self {
        Self { jit }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcLLJITRef {
        self.jit
    }

    /// Return the target triple for this LLJIT instance.
    pub fn get_triple_string(&self) -> &CStr {
        // This string is owned by the LLJIT instance and does not need to be freed by the caller.
        unsafe { CStr::from_ptr(LLVMOrcLLJITGetTripleString(self.jit)) }
    }

    /// Return the data layout for this LLJIT instance.
    pub fn get_data_layout_string(&self) -> &CStr {
        // This string is owned by the LLJIT instance and does not need to be freed by the caller.
        unsafe { CStr::from_ptr(LLVMOrcLLJITGetDataLayoutStr(self.jit)) }
    }

    /// Returns the global prefix character according to the LLJIT's DataLayout.
    pub fn get_global_prefix(&self) -> c_char {
        unsafe { LLVMOrcLLJITGetGlobalPrefix(self.jit) }
    }

    /// Add an IR module to the main JITDylib.
    pub fn add_module(&self, tsm: ThreadSafeModule) -> Result<(), LLVMString> {
        let jd = self.get_main_jit_dylib();
        self.add_module_with_dylib(tsm, jd)
    }

    /// Add an IR module to the given JITDylib.
    pub fn add_module_with_dylib(
        &self,
        tsm: ThreadSafeModule,
        jd: JITDylibRef,
    ) -> Result<(), LLVMString> {
        let tsm = mem::ManuallyDrop::new(tsm);
        cvt(unsafe { LLVMOrcLLJITAddLLVMIRModule(self.as_inner(), jd.as_inner(), tsm.as_inner()) })
    }

    /// Add an IR module to the given ResourceTracker's JITDylib.
    pub fn add_module_with_rt(
        &self,
        tsm: ThreadSafeModule,
        rt: &ResourceTracker,
    ) -> Result<(), LLVMString> {
        let tsm = mem::ManuallyDrop::new(tsm);
        cvt(unsafe {
            LLVMOrcLLJITAddLLVMIRModuleWithRT(self.as_inner(), rt.as_inner(), tsm.as_inner())
        })
    }

    /// Add a relocatable object file to the given ResourceTracker's JITDylib.
    pub fn add_object_with_rt(
        &self,
        name: &CStr,
        object: &[u8],
        rt: &ResourceTracker,
    ) -> Result<(), LLVMString> {
        let buf = unsafe {
            LLVMCreateMemoryBufferWithMemoryRangeCopy(
                object.as_ptr().cast(),
                object.len(),
                name.as_ptr(),
            )
        };
        cvt(unsafe { LLVMOrcLLJITAddObjectFileWithRT(self.as_inner(), rt.as_inner(), buf) })
    }

    /// Gets the execution session.
    pub fn get_execution_session(&self) -> ExecutionSessionRef<'_> {
        unsafe { ExecutionSessionRef::from_inner(LLVMOrcLLJITGetExecutionSession(self.as_inner())) }
    }

    /// Return a reference to the Main JITDylib.
    pub fn get_main_jit_dylib(&self) -> JITDylibRef {
        unsafe { JITDylibRef::from_inner_unchecked(LLVMOrcLLJITGetMainJITDylib(self.as_inner())) }
    }

    /// Mangles the given string according to the LLJIT instance's DataLayout, then interns the
    /// result in the SymbolStringPool and returns a reference to the pool entry.
    pub fn mangle_and_intern(&self, unmangled_name: &CStr) -> SymbolStringPoolEntry {
        unsafe {
            SymbolStringPoolEntry::from_inner_unchecked(LLVMOrcLLJITMangleAndIntern(
                self.as_inner(),
                unmangled_name.as_ptr(),
            ))
        }
    }

    /// Look up the given symbol in the main JITDylib of the given LLJIT instance.
    ///
    /// The name is **unmangled** — `LLVMOrcLLJITLookup` applies the data layout
    /// prefix (e.g. `_` on macOS) internally.
    pub fn lookup(&self, name: &CStr) -> Result<usize, LLVMString> {
        let mut res = MaybeUninit::uninit();
        cvt(unsafe { LLVMOrcLLJITLookup(self.as_inner(), res.as_mut_ptr(), name.as_ptr()) })?;
        Ok(unsafe { res.assume_init() }.try_into().unwrap())
    }

    /// Look up a symbol in a specific JITDylib.
    ///
    /// Unlike [`lookup`](Self::lookup), which only searches the main JITDylib,
    /// this searches the given `jd`. The name is **unmangled** — the LLJIT
    /// applies the data layout prefix (e.g. `_` on macOS) internally.
    pub fn lookup_in(&self, jd: JITDylibRef, name: &CStr) -> Result<usize, LLVMString> {
        let mut res = MaybeUninit::uninit();
        cvt(unsafe {
            crate::cpp::revmc_llvm_lljit_lookup_in(
                self.as_inner(),
                jd.as_inner(),
                res.as_mut_ptr(),
                name.as_ptr(),
            )
        })?;
        Ok(unsafe { res.assume_init() }.try_into().unwrap())
    }

    /// Returns a non-owning reference to the LLJIT instance's IR transform layer.
    pub fn get_ir_transform_layer(&self) -> IRTransformLayerRef {
        unsafe { IRTransformLayerRef::from_inner(LLVMOrcLLJITGetIRTransformLayer(self.as_inner())) }
    }

    /// Returns a non-owning reference to the LLJIT instance's object transform layer.
    pub fn get_obj_transform_layer(&self) -> ObjectTransformLayerRef {
        unsafe {
            ObjectTransformLayerRef::from_inner(LLVMOrcLLJITGetObjTransformLayer(self.as_inner()))
        }
    }

    /// Install `PerfSupportPlugin` on the LLJIT's `ObjectLinkingLayer`.
    ///
    /// Writes perf jitdump records so `perf record -k 1` / `perf inject --jit`
    /// can resolve JIT-compiled symbols with full debug info and unwind info.
    /// Requires `ObjectLinkingLayer` (JITLink), which is the LLJIT default.
    pub fn enable_perf_support(&self) -> Result<(), LLVMString> {
        cvt(unsafe { crate::cpp::revmc_llvm_lljit_enable_perf_support(self.as_inner()) })
    }

    /// Install `SimplePerfSupportPlugin` on the LLJIT's `ObjectLinkingLayer`.
    ///
    /// Writes `/tmp/perf-<pid>.map` in the perf map format so that profilers
    /// like `perf` and `samply` can resolve JIT-compiled symbols without
    /// the heavyweight jitdump machinery.
    /// Requires `ObjectLinkingLayer` (JITLink), which is the LLJIT default.
    ///
    /// Not suitable for long-running programs. The map file is append-only
    /// and never cleaned up, so entries for freed JIT code accumulate
    /// indefinitely. Prefer [`enable_perf_support`](Self::enable_perf_support)
    /// (jitdump) for long-lived processes.
    pub fn enable_simple_perf(&self) -> Result<(), LLVMString> {
        cvt(unsafe { crate::cpp::revmc_llvm_lljit_enable_simple_perf(self.as_inner()) })
    }

    /// Install the plugin that submits debug objects to the executor via the
    /// GDB JIT Interface (`__jit_debug_register_code`).
    ///
    /// On ELF this installs `ELFDebugObjectPlugin`; on MachO,
    /// `GDBJITDebugInfoRegistrationPlugin`. Requires `ObjectLinkingLayer`
    /// (JITLink), which is the LLJIT default.
    ///
    /// Once enabled, compiled objects containing DWARF debug info are
    /// automatically registered so that GDB, LLDB, `perf`, and other profilers
    /// can resolve JIT-compiled function names and source locations.
    pub fn enable_debug_support(&self) -> Result<(), LLVMString> {
        cvt(unsafe { crate::cpp::revmc_llvm_lljit_enable_debug_support(self.as_inner()) })
    }
}

impl Drop for LLJIT {
    fn drop(&mut self) {
        if let Err(e) = cvt(unsafe { LLVMOrcDisposeLLJIT(self.jit) }) {
            error!("Failed to dispose JIT: {e}");
        }
    }
}

/// A reference to an object layer.
pub struct ObjectLayerRef {
    ptr: LLVMOrcObjectLayerRef,
}

impl ObjectLayerRef {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcObjectLayerRef) -> Self {
        Self { ptr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcObjectLayerRef {
        self.ptr
    }
}

/// Callback for [`ObjectTransformLayerRef::set_transform`].
type ObjTransformFn = fn(&[u8]) -> Result<Option<Vec<u8>>, String>;

/// A reference to an object transform layer.
///
/// The transform layer sits between the IR compiler and the object linking layer.
/// A transform callback can inspect or modify the compiled object buffer before linking.
pub struct ObjectTransformLayerRef {
    ptr: LLVMOrcObjectTransformLayerRef,
}

impl ObjectTransformLayerRef {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcObjectTransformLayerRef) -> Self {
        Self { ptr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcObjectTransformLayerRef {
        self.ptr
    }

    // TODO: avoid copying, better api for mutation
    /// Set the transform function on this object transform layer.
    ///
    /// The callback receives the compiled object buffer as `&[u8]`. Returning `Ok(None)`
    /// passes the buffer through unchanged; returning `Ok(Some(new_bytes))` replaces it.
    /// Returning `Err` aborts materialization.
    pub fn set_transform(&self, f: ObjTransformFn) {
        extern "C" fn shim(ctx: *mut c_void, obj_in_out: *mut LLVMMemoryBufferRef) -> LLVMErrorRef {
            use inkwell::llvm_sys::core::{
                LLVMCreateMemoryBufferWithMemoryRangeCopy, LLVMDisposeMemoryBuffer,
                LLVMGetBufferSize, LLVMGetBufferStart,
            };
            let f: ObjTransformFn = unsafe { mem::transmute(ctx) };
            let buf = unsafe { *obj_in_out };
            let data = unsafe {
                let start = LLVMGetBufferStart(buf);
                let size = LLVMGetBufferSize(buf);
                std::slice::from_raw_parts(start.cast::<u8>(), size)
            };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| f(data)));
            cvt_cb_res_t(res, |new_bytes| {
                if let Some(new_bytes) = new_bytes {
                    unsafe {
                        LLVMDisposeMemoryBuffer(buf);
                        *obj_in_out = LLVMCreateMemoryBufferWithMemoryRangeCopy(
                            new_bytes.as_ptr().cast(),
                            new_bytes.len(),
                            c"".as_ptr(),
                        );
                    }
                }
            })
        }

        let ctx = f as *mut c_void;
        unsafe { self.set_transform_raw(shim, ctx) }
    }

    /// Set a raw transform function on this object transform layer.
    ///
    /// # Safety
    ///
    /// `ctx` must be valid for the lifetime of this layer and `f` must not unwind.
    pub unsafe fn set_transform_raw(
        &self,
        f: extern "C" fn(ctx: *mut c_void, obj_in_out: *mut LLVMMemoryBufferRef) -> LLVMErrorRef,
        ctx: *mut c_void,
    ) {
        LLVMOrcObjectTransformLayerSetTransform(self.as_inner(), f, ctx);
    }
}

/// A reference to an IR transform layer.
pub struct IRTransformLayerRef {
    ptr: LLVMOrcIRTransformLayerRef,
}

impl IRTransformLayerRef {
    /// Wraps a raw pointer.
    pub unsafe fn from_inner(ptr: LLVMOrcIRTransformLayerRef) -> Self {
        Self { ptr }
    }

    /// Unwraps the raw pointer.
    pub fn as_inner(&self) -> LLVMOrcIRTransformLayerRef {
        self.ptr
    }

    /// Emit should materialize the given IR.
    pub fn emit(&self, mr: MaterializationResponsibility, tsm: ThreadSafeModule) {
        let mr = mem::ManuallyDrop::new(mr);
        let tsm = mem::ManuallyDrop::new(tsm);
        unsafe { LLVMOrcIRTransformLayerEmit(self.as_inner(), mr.as_inner(), tsm.as_inner()) };
    }

    /// Set the transform function of this transform layer.
    pub fn set_transform(
        &self,
        f: fn(&ThreadSafeModule, MaterializationResponsibilityRef<'_>) -> Result<(), String>,
    ) {
        extern "C" fn shim(
            ctx: *mut c_void,
            m: *mut LLVMOrcThreadSafeModuleRef,
            mr: LLVMOrcMaterializationResponsibilityRef,
        ) -> LLVMErrorRef {
            let f: fn(
                &ThreadSafeModule,
                MaterializationResponsibilityRef<'_>,
            ) -> Result<(), String> = unsafe { mem::transmute(ctx) };
            let m = mem::ManuallyDrop::new(unsafe { ThreadSafeModule::from_inner(*m) });
            let mr = unsafe { MaterializationResponsibilityRef::from_inner(mr) };
            let res = std::panic::catch_unwind(AssertUnwindSafe(|| f(&m, mr)));
            cvt_cb_res(res)
        }

        let ctx = f as *mut c_void;
        unsafe { LLVMOrcIRTransformLayerSetTransform(self.as_inner(), shim, ctx) };
    }
}

// SAFETY: ORC/LLJIT is designed for concurrent use from multiple threads.
// All session operations are internally synchronized.
unsafe impl Send for LLJIT {}
unsafe impl Sync for LLJIT {}

unsafe impl Send for ExecutionSessionRef<'_> {}
unsafe impl Sync for ExecutionSessionRef<'_> {}

unsafe impl Send for JITDylibRef {}
unsafe impl Sync for JITDylibRef {}

unsafe impl Send for ResourceTracker {}
unsafe impl Sync for ResourceTracker {}

unsafe impl Send for ObjectTransformLayerRef {}
unsafe impl Sync for ObjectTransformLayerRef {}

unsafe impl Send for IRTransformLayerRef {}
unsafe impl Sync for IRTransformLayerRef {}

unsafe impl Send for SymbolStringPoolRef {}
unsafe impl Sync for SymbolStringPoolRef {}

unsafe impl Send for SymbolStringPoolEntry {}
unsafe impl Sync for SymbolStringPoolEntry {}

unsafe impl Send for ThreadSafeContext {}

unsafe impl Send for ThreadSafeModule {}

unsafe impl Send for MaterializationUnit {}

unsafe impl Send for DefinitionGenerator {}

unsafe impl Send for JITTargetMachineBuilder {}

unsafe impl Send for LLJITBuilder {}

/// Converts an `LLVMErrorRef` to a `Result`.
pub(crate) fn cvt(ptr: LLVMErrorRef) -> Result<(), LLVMString> {
    if ptr.is_null() { Ok(()) } else { Err(unsafe { llvm_string(LLVMGetErrorMessage(ptr)) }) }
}

fn cvt_cb_res(res: Result<Result<(), String>, Box<dyn std::any::Any + Send>>) -> LLVMErrorRef {
    cvt_cb_res_t(res, drop)
}

fn cvt_cb_res_t<T>(
    res: Result<Result<T, String>, Box<dyn std::any::Any + Send>>,
    f: impl FnOnce(T),
) -> LLVMErrorRef {
    let msg = match res {
        Ok(Ok(x)) => {
            f(x);
            return ptr::null_mut(); // LLVMErrorSuccess
        }
        Ok(Err(e)) => e,
        Err(e) => format!("callback panicked, payload: {:?}", panic_payload(&e)),
    };
    unsafe { LLVMCreateStringError(CString::new(msg).unwrap_or_default().as_ptr()) }
}

fn panic_payload(any: &dyn std::any::Any) -> Option<&str> {
    if let Some(s) = any.downcast_ref::<&str>() {
        Some(*s)
    } else if let Some(s) = any.downcast_ref::<String>() {
        Some(s.as_str())
    } else {
        None
    }
}

/// Deallocates the vector without running the elements' destructors.
// Comment from LLVMOrcAbsoluteSymbols:
/*
 * This function takes ownership of the elements of the Syms array. The Name
 * fields of the array elements are taken to have been retained for this
 * function. This allows the following pattern...
 *
 *   size_t NumPairs;
 *   LLVMOrcCSymbolMapPairs Sym;
 *   -- Build Syms array --
 *   LLVMOrcMaterializationUnitRef MU =
 *       LLVMOrcAbsoluteSymbols(Syms, NumPairs);
 *
 * ... without requiring cleanup of the elements of the Sym array afterwards.
 *
 * The client is still responsible for deleting the Sym array itself.
 *
 * If a client wishes to reuse elements of the Sym array after this call they
 * must explicitly retain each of the elements for themselves.
 */
struct ManuallyDropElements<T> {
    vec: Vec<T>,
}

impl<T> ManuallyDropElements<T> {
    #[inline(always)]
    fn new(vec: Vec<T>) -> Self {
        Self { vec }
    }
}

impl<T> std::ops::Deref for ManuallyDropElements<T> {
    type Target = Vec<T>;

    #[inline(always)]
    fn deref(&self) -> &Vec<T> {
        &self.vec
    }
}

impl<T> std::ops::DerefMut for ManuallyDropElements<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.vec
    }
}

impl<T> Drop for ManuallyDropElements<T> {
    #[inline(always)]
    fn drop(&mut self) {
        unsafe { self.vec.set_len(0) };
    }
}

#[cfg(test)]
mod tests {
    use inkwell::{passes::PassBuilderOptions, targets::Target};

    use super::*;

    #[test]
    fn e2e_with_tsc() {
        use mem::ManuallyDrop;

        Target::initialize_native(&Default::default()).unwrap();
        let triple = TargetMachine::get_default_triple();
        let cpu = TargetMachine::get_host_cpu_name();
        let features = TargetMachine::get_host_cpu_features();
        let target = Target::from_triple(&triple).unwrap();

        let cx = Context::create();
        let raw = cx.raw();
        let tscx = ThreadSafeContext::from_context(cx);
        let cx_handle = Box::new(ManuallyDrop::new(unsafe { Context::new(raw) }));
        let cx: &Context = unsafe { &*(&**cx_handle as *const Context) };

        let machine = target
            .create_target_machine(
                &triple,
                &cpu.to_string_lossy(),
                &features.to_string_lossy(),
                inkwell::OptimizationLevel::None,
                inkwell::targets::RelocMode::Static,
                inkwell::targets::CodeModel::JITDefault,
            )
            .unwrap();

        let jit = LLJITBuilder::new().build().unwrap();

        // Build module.
        let m = cx.create_module("test");
        m.set_data_layout(&machine.get_target_data().get_data_layout());
        m.set_triple(&triple);

        let fn_name = "my_fn";
        let bcx = cx.create_builder();

        // Add an unused external declaration and register absolute symbol.
        extern "C" fn my_external(x: i64) -> i64 {
            x + 100
        }
        let ext_ty = cx.i64_type().fn_type(&[cx.i64_type().into()], false);
        let _ext_fn =
            m.add_function("my_external", ext_ty, Some(inkwell::module::Linkage::External));
        let ext_name = CString::new("my_external").unwrap();
        let sym = SymbolMapPair::new(
            jit.mangle_and_intern(&ext_name),
            EvaluatedSymbol::new(
                my_external as *const () as u64,
                SymbolFlags::none().exported().callable(),
            ),
        );
        jit.get_main_jit_dylib()
            .define(MaterializationUnit::absolute_symbols(vec![sym]))
            .map_err(|(e, _)| e)
            .unwrap();

        let ty = cx.i64_type().fn_type(&[], false);
        let f = m.add_function(fn_name, ty, Some(inkwell::module::Linkage::External));
        let bb = cx.append_basic_block(f, "entry");
        bcx.position_at_end(bb);
        bcx.build_return(Some(&cx.i64_type().const_int(142, false))).unwrap();

        eprintln!("module:\n{}", m.print_to_string().to_string_lossy());
        m.verify().unwrap();

        // Wrap and add to JIT.
        let tsm = tscx.create_module(m);
        jit.add_module(tsm).unwrap();

        // Lookup.
        let address = jit.lookup(&CString::new(fn_name).unwrap()).unwrap();
        eprintln!("address: {address:#x}");
        // Check the first few bytes at the address to see if it's valid code.
        let code = unsafe { std::slice::from_raw_parts(address as *const u8, 16) };
        eprintln!("code bytes: {code:02x?}");
        let f = unsafe { mem::transmute::<usize, extern "C" fn() -> u64>(address) };
        eprintln!("about to call...");
        let r = f();
        eprintln!("result: {r}");
        assert_eq!(r, 142);
        eprintln!("dropping jit...");
        drop(jit);
        eprintln!("dropping machine...");
        drop(machine);
        eprintln!("dropping cx_handle...");
        drop(cx_handle);
        eprintln!("dropping tscx...");
        drop(tscx);
        eprintln!("all dropped, test done");
    }

    #[test]
    fn lookup_in_custom_jd() {
        use mem::ManuallyDrop;

        Target::initialize_native(&Default::default()).unwrap();
        let triple = TargetMachine::get_default_triple();
        let cpu = TargetMachine::get_host_cpu_name();
        let features = TargetMachine::get_host_cpu_features();
        let target = Target::from_triple(&triple).unwrap();

        let cx = Context::create();
        let raw = cx.raw();
        let tscx = ThreadSafeContext::from_context(cx);
        let cx_handle = Box::new(ManuallyDrop::new(unsafe { Context::new(raw) }));
        let cx: &Context = unsafe { &*(&**cx_handle as *const Context) };

        let machine = target
            .create_target_machine(
                &triple,
                &cpu.to_string_lossy(),
                &features.to_string_lossy(),
                inkwell::OptimizationLevel::None,
                inkwell::targets::RelocMode::Static,
                inkwell::targets::CodeModel::JITDefault,
            )
            .unwrap();

        let jit = LLJITBuilder::new().build().unwrap();

        // Create two custom JITDylibs with functions returning different values.
        let jd_a = jit.get_execution_session().create_bare_jit_dylib(c"test_a");
        let jd_b = jit.get_execution_session().create_bare_jit_dylib(c"test_b");

        for (jd, value) in [(&jd_a, 111u64), (&jd_b, 222u64)] {
            let m = cx.create_module("test");
            m.set_data_layout(&machine.get_target_data().get_data_layout());
            m.set_triple(&triple);

            let fn_name = "test_fn";
            let bcx = cx.create_builder();
            let ty = cx.i64_type().fn_type(&[], false);
            let f = m.add_function(fn_name, ty, Some(inkwell::module::Linkage::External));
            let bb = cx.append_basic_block(f, "entry");
            bcx.position_at_end(bb);
            bcx.build_return(Some(&cx.i64_type().const_int(value, false))).unwrap();
            m.verify().unwrap();

            let module = ManuallyDrop::new(m);
            let tsm = unsafe {
                ThreadSafeModule::create_in_context(Module::new(module.as_mut_ptr()), &tscx)
            };
            jit.add_module_with_dylib(tsm, unsafe {
                JITDylibRef::from_inner_unchecked(jd.as_inner())
            })
            .unwrap();
        }

        // Lookup in each JD should return the correct function.
        let addr_a = jit.lookup_in(jd_a, c"test_fn").unwrap();
        let addr_b = jit.lookup_in(jd_b, c"test_fn").unwrap();
        assert_ne!(addr_a, addr_b);

        let f_a = unsafe { mem::transmute::<usize, extern "C" fn() -> u64>(addr_a) };
        let f_b = unsafe { mem::transmute::<usize, extern "C" fn() -> u64>(addr_b) };
        assert_eq!(f_a(), 111);
        assert_eq!(f_b(), 222);

        // Clearing one JD should not affect the other.
        jd_a.clear().unwrap();
        assert_eq!(f_b(), 222);
    }

    #[test]
    fn e2e() {
        let tsm = ThreadSafeModule::create("test");
        let fn_name = "my_fn";
        tsm.with_module(|m| {
            let cx = m.get_context();
            let bcx = cx.create_builder();
            let ty = cx.i64_type().fn_type(&[], false);

            let f = m.add_function(fn_name, ty, Some(inkwell::module::Linkage::External));
            let bb = cx.append_basic_block(f, "entry");
            bcx.position_at_end(bb);
            bcx.build_int_compare(
                inkwell::IntPredicate::EQ,
                cx.i64_type().const_zero(),
                cx.i64_type().const_all_ones(),
                "a",
            )
            .unwrap();
            bcx.build_return(Some(&cx.i64_type().const_int(69, false))).unwrap();

            eprintln!("--before--");
            eprintln!("{}", m.print_to_string().to_string_lossy());

            m.verify().map_err(|e| e.to_string())?;

            Target::initialize_native(&Default::default()).unwrap();
            let triple = TargetMachine::get_default_triple();
            let cpu = TargetMachine::get_host_cpu_name();
            let features = TargetMachine::get_host_cpu_features();
            let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
            let machine = target
                .create_target_machine(
                    &triple,
                    &cpu.to_string_lossy(),
                    &features.to_string_lossy(),
                    Default::default(),
                    inkwell::targets::RelocMode::Default,
                    inkwell::targets::CodeModel::Default,
                )
                .ok_or_else(|| String::from("failed to create target machine"))?;

            m.run_passes("default<O3>", &machine, PassBuilderOptions::create()).unwrap();

            eprintln!("--after--");
            eprintln!("{}", m.print_to_string().to_string_lossy());

            Ok(())
        })
        .unwrap();

        let jit = LLJIT::new().unwrap();
        jit.add_module(tsm).unwrap();
        let address = jit.lookup(&CString::new(fn_name).unwrap()).unwrap();
        eprintln!("address: {address:#x}");
        let f = unsafe { mem::transmute::<usize, extern "C" fn() -> u64>(address) };
        let r = f();
        assert_eq!(r, 69);
    }
}
