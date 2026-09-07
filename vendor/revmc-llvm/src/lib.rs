#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
extern crate tracing;

use std::{
    cell::Cell,
    ffi::{CStr, CString},
    fmt::{self, Write},
    iter,
    mem::ManuallyDrop,
    path::Path,
    sync::{
        Arc, Once, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use alloy_primitives::map::{FxBuildHasher, HashSet};
pub use inkwell::{self, context::Context};
use inkwell::{
    AddressSpace, IntPredicate,
    attributes::{Attribute, AttributeLoc},
    basic_block::BasicBlock,
    debug_info::{
        AsDIScope, DICompileUnit, DIFlags, DIFlagsConstants, DISubprogram, DWARFEmissionKind,
        DWARFSourceLanguage, DebugInfoBuilder,
    },
    module::{FlagBehavior, Module},
    passes::PassBuilderOptions,
    support::error_handling::install_fatal_error_handler,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
    types::{
        AnyType, AnyTypeEnum, BasicType, BasicTypeEnum, FunctionType, IntType, PointerType,
        StringRadix, VoidType,
    },
    values::{
        AsValueRef, BasicMetadataValueEnum, BasicValue, BasicValueEnum, CallSiteValue,
        FunctionValue, InstructionValue, PointerValue,
    },
};
use object::{Object, ObjectSymbol};
use revmc_backend::{
    Backend, BackendConfig, BackendTypes, Builder, CallConv, IntCC, OptimizationLevel, Result,
    TailCallKind, TypeMethods, U256, eyre, format_bytes,
};

mod cpp;

mod dh;
pub mod orc;

mod utils;
pub(crate) use utils::*;

/// Branch weight for non-cold branches.
const DEFAULT_WEIGHT: u32 = 20000;

/// Current JIT memory usage counters.
///
/// Counters reflect live memory: bytes are added on compilation and
/// subtracted when JIT code is freed.
#[derive(Clone, Copy, Debug, Default)]
pub struct JitMemoryUsage {
    /// Bytes allocated for executable (code) sections.
    pub code_bytes: usize,
    /// Bytes allocated for non-executable (data) sections.
    pub data_bytes: usize,
}

impl JitMemoryUsage {
    /// Total bytes (code + data).
    pub fn total_bytes(&self) -> usize {
        self.code_bytes + self.data_bytes
    }
}

impl fmt::Display for JitMemoryUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "total: {}, code: {}, data: {}",
            format_bytes(self.total_bytes()),
            format_bytes(self.code_bytes),
            format_bytes(self.data_bytes),
        )
    }
}

/// Atomic counters written by the C++ `MemoryUsagePlugin`.
///
/// Leaked into a `&'static` reference so the plugin (which lives as long as
/// the process-global LLJIT) always has valid pointers.
struct JitMemoryCounters {
    code_bytes: AtomicUsize,
    data_bytes: AtomicUsize,
}

impl JitMemoryCounters {
    fn get(&self) -> JitMemoryUsage {
        JitMemoryUsage {
            code_bytes: self.code_bytes.load(Ordering::Relaxed),
            data_bytes: self.data_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Returns the current JIT memory usage.
///
/// Returns `None` if the JIT has not been initialized yet.
pub fn jit_memory_usage() -> Option<JitMemoryUsage> {
    GlobalOrcJit::try_get().map(|g| g.memory_counters.get())
}

/// Collect garbage on the global JIT.
pub fn global_gc() {
    if let Some(orc) = GlobalOrcJit::try_get() {
        orc.gc();
    }
}

type FxHashMap<K, V> = alloy_primitives::map::HashMap<K, V, FxBuildHasher>;

/// The LLVM-based EVM bytecode compiler backend.
#[derive(Debug)]
#[must_use]
pub struct EvmLlvmBackend {
    cx: &'static Context,
    _dh: dh::DiagnosticHandlerGuard,
    bcx: inkwell::builder::Builder<'static>,
    module: Module<'static>,
    machine: TargetMachine,

    /// ORC JIT state. `None` in AOT mode.
    /// Dropped before `_tscx` so the JIT engine is disposed before the context.
    orc: Option<OrcJitState>,
    /// ORC thread-safe context that owns the LLVM context (JIT mode only).
    _tscx: Option<orc::ThreadSafeContext>,
    /// Non-owning context handle for JIT mode. See [`create_orc_context`].
    _cx_handle: Option<Box<ManuallyDrop<Context>>>,
    /// Owned context for AOT mode. `None` in JIT mode.
    _aot_cx: Option<Box<Context>>,

    /// LLVM debug info builder and compile unit, created lazily when `debug_file` is set.
    di_state: Option<DiState>,

    ty_void: VoidType<'static>,
    ty_ptr: PointerType<'static>,
    ty_i1: IntType<'static>,
    ty_i8: IntType<'static>,
    ty_i32: IntType<'static>,
    ty_i64: IntType<'static>,
    ty_i256: IntType<'static>,
    ty_isize: IntType<'static>,

    aot: bool,
    backend_config: BackendConfig,
    scratch: String,
    /// Separate from `function_names` to have always increasing IDs.
    function_counter: u32,
    /// Persistent mapping from function ID to symbol name.
    function_names: FxHashMap<u32, String>,
}

// Thread-local slot for capturing compiled object buffers from the ObjectTransformLayer.
// Safe because LLJIT uses InPlaceTaskDispatcher — compilation runs inline on the calling thread.
thread_local! {
    static OBJ_CAPTURE: Cell<*mut Option<Vec<u8>>> = const { Cell::new(std::ptr::null_mut()) };
}

/// RAII guard that arms the thread-local object capture for the duration of a JIT commit.
struct ScopedObjCapture {
    prev: *mut Option<Vec<u8>>,
}

impl ScopedObjCapture {
    fn install(slot: &mut Option<Vec<u8>>) -> Self {
        Self { prev: OBJ_CAPTURE.replace(slot as *mut _) }
    }
}

impl Drop for ScopedObjCapture {
    fn drop(&mut self) {
        OBJ_CAPTURE.set(self.prev);
    }
}

fn obj_capture_transform(obj: &[u8]) -> Result<Option<Vec<u8>>, String> {
    OBJ_CAPTURE.with(|tls| {
        let ptr = tls.get();
        if !ptr.is_null() {
            unsafe { *ptr = Some(obj.to_vec()) };
        }
    });
    Ok(None)
}

/// Process-global shared LLJIT instance.
///
/// ORC/LLJIT is thread-safe and designed to be shared. Individual compilers get
/// their own [`JITDylib`](orc::JITDylibRef) for symbol isolation.
///
/// Builtin function pointers (absolute symbols) are defined once in a shared
/// `builtins` JITDylib. Each per-compiler JITDylib links against the builtins
/// JD so compiled code can resolve them without duplicating definitions.
struct GlobalOrcJit {
    /// Shared JITDylib containing absolute symbols for builtin functions.
    /// Added to each per-compiler JITDylib's link order.
    builtins_jd: orc::JITDylibRef,

    /// Symbols already defined in the builtins JD.
    builtins_defined: std::sync::Mutex<HashSet<CString>>,

    next_dylib_id: AtomicU64,

    /// Live JIT memory counters, updated by the C++ MemoryUsagePlugin.
    memory_counters: &'static JitMemoryCounters,

    jit: orc::LLJIT,
}

impl GlobalOrcJit {
    fn global() -> &'static OnceLock<Result<Self, String>> {
        static GLOBAL: OnceLock<Result<GlobalOrcJit, String>> = OnceLock::new();
        &GLOBAL
    }

    fn try_get() -> Option<&'static Self> {
        Self::global().get().and_then(|r| r.as_ref().ok())
    }

    fn get(
        debug_support: bool,
        profiling_support: bool,
        simple_perf: bool,
    ) -> Result<&'static Self> {
        let result = Self::global().get_or_init(|| {
            init().map_err(|e| e.to_string())?;
            let jit =
                orc::LLJIT::builder().concurrent_compiler().build().map_err(|e| e.to_string())?;
            jit.get_execution_session().set_default_error_reporter();
            jit.get_obj_transform_layer().set_transform(obj_capture_transform);

            // Register JIT debug info with debuggers and profilers.
            if debug_support && let Err(e) = jit.enable_debug_support() {
                warn!("failed to enable JIT debug support: {e}");
            }
            if profiling_support && let Err(e) = jit.enable_perf_support() {
                warn!("failed to enable JIT perf support: {e}");
            }
            if simple_perf && let Err(e) = jit.enable_simple_perf() {
                warn!("failed to enable simple perf map support: {e}");
            }

            // Track JIT memory usage.
            let memory_counters: &'static JitMemoryCounters =
                Box::leak(Box::new(JitMemoryCounters {
                    code_bytes: AtomicUsize::new(0),
                    data_bytes: AtomicUsize::new(0),
                }));
            orc::cvt(unsafe {
                cpp::revmc_llvm_lljit_enable_memory_usage(
                    jit.as_inner(),
                    &memory_counters.code_bytes,
                    &memory_counters.data_bytes,
                )
            })
            .map_err(|e| e.to_string())?;

            let builtins_jd = jit.get_execution_session().create_bare_jit_dylib(c"revmc.builtins");

            Ok(Self {
                jit,
                builtins_jd,
                builtins_defined: Default::default(),
                next_dylib_id: Default::default(),
                memory_counters,
            })
        });
        match result {
            Ok(g) => Ok(g),
            Err(e) => Err(eyre::eyre!("{e}")),
        }
    }

    /// Creates a fresh JITDylib for a compiler.
    fn create_jit_dylib(&self) -> orc::JITDylibRef {
        let id = self.next_dylib_id.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!("revmc.compiler.{id}")).unwrap();
        let es = self.jit.get_execution_session();
        let jd = es.create_bare_jit_dylib(&name);
        // Link against the builtins JD so compiled code can resolve builtin symbols.
        jd.add_to_link_order(self.builtins_jd);
        // Attach a process symbol generator so the JITDylib can resolve libc and other
        // process-level symbols (e.g. printf, memcpy) that JIT-compiled code may reference.
        let prefix = self.jit.get_global_prefix();
        if let Ok(generator) = orc::DefinitionGenerator::for_current_process(prefix) {
            jd.add_generator(generator);
        }
        jd
    }

    /// Removes a JITDylib from the ExecutionSession, freeing all its resources.
    fn remove_jit_dylib(&self, jd: orc::JITDylibRef) {
        let es = self.jit.get_execution_session();
        if let Err(e) = es.remove_jit_dylib(jd) {
            error!("failed to remove JITDylib: {e}");
        }
        // Reclaim unreferenced interned strings after removing symbols.
        es.get_symbol_string_pool().clear_dead_entries();
    }

    /// Defines absolute symbols in the shared builtins JITDylib, skipping any
    /// that are already defined.
    fn define_builtins(&self, symbols: &[(CString, usize)]) {
        if symbols.is_empty() {
            return;
        }
        let mut defined = self.builtins_defined.lock().unwrap();
        let new_syms: Vec<_> = symbols
            .iter()
            .filter(|(name, _)| defined.insert(name.clone()))
            .map(|(name, addr)| {
                orc::SymbolMapPair::new(
                    self.jit.mangle_and_intern(name),
                    orc::EvaluatedSymbol::new(
                        *addr as u64,
                        orc::SymbolFlags::none().exported().callable(),
                    ),
                )
            })
            .collect();
        drop(defined);
        if !new_syms.is_empty()
            && let Err((e, _)) =
                self.builtins_jd.define(orc::MaterializationUnit::absolute_symbols(new_syms))
        {
            error!("failed to define builtins: {e}");
        }
    }

    fn gc(&self) {
        self.jit.get_execution_session().get_symbol_string_pool().clear_dead_entries();
    }
}

/// Shared guard that keeps a JITDylib alive.
///
/// The JITDylib will not be cleared or returned to the pool until the last
/// `JitDylibGuard` is dropped. Runtime callers holding JIT function pointers
/// must retain a clone of this guard to prevent the code from being freed.
#[allow(missing_debug_implementations)]
pub struct JitDylibGuard {
    global: &'static GlobalOrcJit,
    jd: orc::JITDylibRef,
}

impl Drop for JitDylibGuard {
    fn drop(&mut self) {
        self.global.remove_jit_dylib(self.jd);
    }
}

/// ORC JIT state for the LLVM backend (JIT mode only).
///
/// The LLVM context is owned separately (via `tscx`) and persists across JIT resets.
/// Each compiler gets its own JITDylib in the global LLJIT for symbol isolation.
struct OrcJitState {
    /// Reference to the global LLJIT instance.
    global: &'static GlobalOrcJit,
    /// Shared guard that owns the JITDylib. The JD is not recycled until all
    /// `Arc<JitDylibGuard>` holders (including external callers) are dropped.
    ///
    /// Declared before `loaded_trackers` so it drops first: `removeJITDylib`
    /// (called in `JitDylibGuard::drop`) clears the JITDylib internally and
    /// must run while the tracker handles are still live.
    jd_guard: Arc<JitDylibGuard>,
    /// Functions in the current staging module (not yet committed to JIT).
    staged_functions: FxHashMap<u32, FunctionValue<'static>>,
    /// Absolute symbols collected during translation, flushed to the global
    /// builtins JITDylib before commit.
    pending_symbols: Vec<(CString, usize)>,
    /// Resource trackers for committed JIT modules, used for code removal.
    loaded_trackers: Vec<orc::ResourceTracker>,
    /// Maps committed function ID → index into `loaded_trackers`.
    committed_functions: FxHashMap<u32, usize>,
    /// Cached object buffer from the last `commit_staged_module`, captured via
    /// ObjectTransformLayer.
    last_compiled_object: Option<Vec<u8>>,
    /// Counter for throttling `SymbolStringPool::clearDeadEntries()` calls.
    clear_pool_counter: u32,
}

impl fmt::Debug for OrcJitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrcJitState")
            .field("staged_functions", &self.staged_functions.len())
            .field("pending_symbols", &self.pending_symbols.len())
            .field("loaded_trackers", &self.loaded_trackers.len())
            .field("committed_functions", &self.committed_functions.len())
            .finish_non_exhaustive()
    }
}

impl OrcJitState {
    fn new(debug_support: bool, profiling_support: bool, simple_perf: bool) -> Result<Self> {
        let global = GlobalOrcJit::get(debug_support, profiling_support, simple_perf)?;
        let jd = global.create_jit_dylib();
        let jd_guard = Arc::new(JitDylibGuard { global, jd });
        Ok(Self {
            global,
            staged_functions: FxHashMap::default(),
            pending_symbols: Vec::new(),
            loaded_trackers: Vec::new(),
            committed_functions: FxHashMap::default(),
            last_compiled_object: None,
            jd_guard,
            clear_pool_counter: 0,
        })
    }

    /// Clears all code and symbols from this compiler's JITDylib.
    fn clear(&mut self) -> Result<()> {
        self.staged_functions.clear();
        self.pending_symbols.clear();
        self.committed_functions.clear();
        self.last_compiled_object = None;
        // Clear the JITDylib before dropping resource trackers: `LLVMOrcJITDylibClear` calls
        // remove on all trackers associated with the dylib and must run while the tracker
        // handles are still live.
        self.jd().clear().map_err(error_msg)?;
        self.loaded_trackers.clear();
        Ok(())
    }

    /// Periodically clears dead entries from the global SymbolStringPool.
    ///
    /// `clearDeadEntries` is O(pool_size), so we throttle it to avoid
    /// O(N²) total cost over many compilations.
    fn maybe_clear_dead_pool_entries(&mut self) {
        self.clear_pool_counter += 1;
        if self.clear_pool_counter.is_multiple_of(256) {
            self.global.jit.get_execution_session().get_symbol_string_pool().clear_dead_entries();
        }
    }

    fn jd(&self) -> orc::JITDylibRef {
        self.jd_guard.jd
    }
}

fn link_jit_object_in_dylib(
    orc: &OrcJitState,
    jd: orc::JITDylibRef,
    symbol_name: &CStr,
    object: &[u8],
    symbols: &[(CString, usize)],
) -> Result<(usize, orc::ResourceTracker)> {
    orc.global.define_builtins(symbols);
    let tracker = jd.create_resource_tracker();
    orc.global.jit.add_object_with_rt(symbol_name, object, &tracker).map_err(error_msg)?;
    let addr = orc.global.jit.lookup_in(jd, symbol_name).map_err(error_msg)?;
    Ok((addr, tracker))
}

/// Wraps a module in a [`orc::ThreadSafeModule`] for transfer to LLJIT.
///
/// Uses a raw pointer cast to work around `Module<'ctx>` invariance — the module's
/// context is genuinely owned by `tscx`.
fn create_thread_safe_module(
    tscx: &orc::ThreadSafeContext,
    module: Module<'static>,
) -> orc::ThreadSafeModule {
    let module = std::mem::ManuallyDrop::new(module);
    // SAFETY: The module was created in the context owned by `tscx`.
    unsafe { orc::ThreadSafeModule::create_in_context(Module::new(module.as_mut_ptr()), tscx) }
}

/// Creates an ORC-owned LLVM context, returning a `&'static` reference to it.
///
/// In JIT mode, ORC owns the context via a [`orc::ThreadSafeContext`] so that modules can be
/// safely transferred to the JIT. A non-owning handle ([`ManuallyDrop<Context>`]) is
/// heap-allocated to provide a stable address for the `&'static` reference.
fn create_orc_context() -> (&'static Context, orc::ThreadSafeContext, Box<ManuallyDrop<Context>>) {
    let cx = Context::create();
    let raw = cx.raw();
    let tscx = orc::ThreadSafeContext::from_context(cx);
    // SAFETY: The TSC now owns the context. `from_context` uses `ManuallyDrop` internally,
    // so the LLVM context is still valid — ownership was just transferred to the TSC.
    let cx_handle = Box::new(ManuallyDrop::new(unsafe { Context::new(raw) }));
    // SAFETY: The Box provides a stable heap address. The context is valid as long as
    // the TSC lives.
    let cx: &'static Context = unsafe { &*(&**cx_handle as *const Context) };
    (cx, tscx, cx_handle)
}

/// LLVM debug info state for a module.
struct DiState {
    dibuilder: DebugInfoBuilder<'static>,
    compile_unit: DICompileUnit<'static>,
    finalized: bool,
}

impl fmt::Debug for DiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiState").field("finalized", &self.finalized).finish()
    }
}

// SAFETY: In JIT mode the LLVM context is owned by an ORC ThreadSafeContext.
// In AOT mode the context is owned by `_aot_cx` (a Box<Context>).
// Both travel with the backend, so no thread-local or shared state is referenced.
unsafe impl Send for EvmLlvmBackend {}

impl EvmLlvmBackend {
    /// Creates a new LLVM backend for the host machine.
    #[instrument(name = "new_llvm_backend", level = "debug", skip_all)]
    pub fn new(aot: bool) -> Result<Self> {
        let config = BackendConfig::default();
        init()?;

        let target_info = TargetInfo::new();
        let target = &target_info.target;
        let machine = target
            .create_target_machine(
                &target_info.triple,
                &target_info.cpu,
                &target_info.features,
                convert_opt_level(config.opt_level),
                if aot { RelocMode::PIC } else { RelocMode::Static },
                CodeModel::Default,
            )
            .ok_or_else(|| eyre::eyre!("failed to create target machine"))?;

        // In JIT mode, ORC owns the context via a ThreadSafeContext so that modules can be
        // safely transferred to the JIT without double-ownership issues with the TLS context.
        // In AOT mode, we use an owned Box<Context> so the backend is safely Send.
        let (cx, tscx, cx_handle, aot_cx) = if aot {
            let aot_cx = Box::new(Context::create());
            // SAFETY: The Box provides a stable heap address. The context is valid as long as
            // `_aot_cx` lives, and it is dropped after all LLVM objects due to field ordering.
            let cx: &'static Context = unsafe { &*(&*aot_cx as *const Context) };
            (cx, None, None, Some(aot_cx))
        } else {
            if !target.has_jit() {
                return Err(eyre::eyre!("target {:?} does not support JIT", target.get_name()));
            }
            if !target.has_target_machine() {
                return Err(eyre::eyre!(
                    "target {:?} does not have target machine",
                    target.get_name()
                ));
            }

            let (cx, tscx, cx_handle) = create_orc_context();
            (cx, Some(tscx), Some(cx_handle), None)
        };

        let module = create_module(cx, &machine, aot)?;
        let bcx = cx.create_builder();

        let ty_void = cx.void_type();
        let ty_i1 = cx.bool_type();
        let ty_i8 = cx.i8_type();
        let ty_i32 = cx.i32_type();
        let ty_i64 = cx.i64_type();
        let ty_i256 =
            cx.custom_width_int_type(std::num::NonZeroU32::new(256).unwrap()).expect("i256");
        let ty_isize = cx.ptr_sized_int_type(&machine.get_target_data(), None);
        let ty_ptr = cx.ptr_type(AddressSpace::default());
        Ok(Self {
            cx,
            _dh: dh::DiagnosticHandlerGuard::new(cx),
            bcx,
            module,
            machine,
            orc: None,
            _tscx: tscx,
            _cx_handle: cx_handle,
            _aot_cx: aot_cx,
            ty_void,
            ty_i1,
            ty_i8,
            ty_i32,
            ty_i64,
            ty_i256,
            ty_isize,
            ty_ptr,
            aot,
            backend_config: config,
            scratch: String::new(),
            function_counter: 0,
            function_names: FxHashMap::default(),
            di_state: None,
        })
    }

    /// Returns the LLVM context.
    #[inline]
    pub fn cx(&self) -> &Context {
        self.cx
    }

    #[inline]
    fn module(&self) -> &Module<'static> {
        &self.module
    }

    fn ensure_orc(&mut self) -> Result<&mut OrcJitState> {
        if self.orc.is_none() {
            self.orc = Some(OrcJitState::new(
                self.backend_config.debug_support,
                self.backend_config.profiling_support,
                self.backend_config.simple_perf,
            )?);
        }
        Ok(self.orc.as_mut().unwrap())
    }

    fn fn_type(
        &self,
        ret: Option<BasicTypeEnum<'static>>,
        params: &[BasicTypeEnum<'static>],
    ) -> FunctionType<'static> {
        let params = params.iter().copied().map(Into::into).collect::<Vec<_>>();
        match ret {
            Some(ret) => ret.fn_type(&params, false),
            None => self.ty_void.fn_type(&params, false),
        }
    }

    /// Returns the given name if IR output is being dumped, otherwise an empty string.
    /// LLVM skips internal name processing for empty names, avoiding overhead when names
    /// are not needed for readability.
    #[inline]
    fn name<'a>(&self, name: &'a str) -> &'a str {
        if self.backend_config.is_dumping { name } else { "" }
    }

    fn id_to_name(&self, id: u32) -> &str {
        &self.function_names[&id]
    }

    /// Lazily initializes the debug info builder and compile unit for the module.
    fn ensure_di_state(&mut self) {
        if self.di_state.is_some() {
            return;
        }
        let Some(debug_file) = &self.backend_config.debug_file else { return };

        let filename =
            debug_file.file_name().map(|f| f.to_string_lossy()).unwrap_or_default().into_owned();
        let directory =
            debug_file.parent().map(|p| p.to_string_lossy()).unwrap_or_default().into_owned();

        // Add required module flags for debug info.
        self.module().add_basic_value_flag(
            "Debug Info Version",
            FlagBehavior::Warning,
            self.ty_i32.const_int(inkwell::debug_info::debug_metadata_version() as u64, false),
        );
        self.module().add_basic_value_flag(
            "Dwarf Version",
            FlagBehavior::Warning,
            self.ty_i32.const_int(5, false),
        );

        let opt_level = self.backend_config.opt_level;
        let is_optimized = opt_level != OptimizationLevel::None;
        let mut flags = Vec::new();
        flags.push(match opt_level {
            OptimizationLevel::None => "-O0",
            OptimizationLevel::Less => "-O1",
            OptimizationLevel::Default => "-O2",
            OptimizationLevel::Aggressive => "-O3",
        });
        flags.push(if self.aot { "--aot" } else { "--jit" });
        let flags = flags.join(" ");

        let (dibuilder, compile_unit) = self.module().create_debug_info_builder(
            true,
            DWARFSourceLanguage::C,
            &filename,
            &directory,
            "revmc",
            is_optimized,
            &flags,
            0,
            "",
            DWARFEmissionKind::Full,
            0,
            false,
            false,
            "",
            "",
        );

        self.di_state = Some(DiState { dibuilder, compile_unit, finalized: false });
    }

    /// Commits the current staging module to the ORC JIT if there are pending functions.
    fn commit_staged_module(&mut self) -> Result<()> {
        if self.aot || self.orc.as_ref().is_none_or(|o| o.staged_functions.is_empty()) {
            return Ok(());
        }

        self.di_state = None;

        let new_module = create_module(self.cx, &self.machine, self.aot)?;
        let old_module = std::mem::replace(&mut self.module, new_module);

        let tscx = self._tscx.as_ref().expect("missing ThreadSafeContext");
        let orc = self.orc.as_mut().unwrap();

        // Flush pending absolute symbols to the shared builtins JITDylib.
        let pending = &mut orc.pending_symbols;
        if !pending.is_empty() {
            orc.global.define_builtins(pending);
            pending.clear();
        }

        let tracker = orc.jd().create_resource_tracker();

        let tsm = create_thread_safe_module(tscx, old_module);
        orc.global.jit.add_module_with_rt(tsm, &tracker).map_err(error_msg)?;

        let tracker_idx = orc.loaded_trackers.len();
        for &id in orc.staged_functions.keys() {
            orc.committed_functions.insert(id, tracker_idx);
        }
        orc.loaded_trackers.push(tracker);
        orc.staged_functions.clear();
        Ok(())
    }

    /// Clears all code from this compiler's JITDylib, freeing JIT-compiled code.
    /// The LLVM context and global LLJIT are reused across resets.
    fn reset_jit(&mut self) -> Result<()> {
        self.function_names.clear();
        self.di_state = None;

        if let Some(orc) = &mut self.orc {
            orc.clear()?;
        }

        self.module = create_module(self.cx, &self.machine, self.aot)?;

        Ok(())
    }

    /// Returns pending absolute symbols collected while translating the current JIT module.
    pub fn pending_symbol_names(&self) -> Vec<CString> {
        self.orc
            .as_ref()
            .map(|orc| orc.pending_symbols.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// Links a relocatable object into this backend's JITDylib and returns the function address
    /// and resource tracker that owns the linked code.
    pub fn link_jit_object(
        &mut self,
        symbol_name: &CStr,
        object: &[u8],
        symbols: &[(CString, usize)],
    ) -> Result<(usize, orc::ResourceTracker)> {
        let orc = self.ensure_orc()?;
        let (addr, tracker) =
            link_jit_object_in_dylib(orc, orc.jd(), symbol_name, object, symbols)?;
        orc.loaded_trackers.push(tracker);
        Ok((addr, orc.loaded_trackers.pop().unwrap()))
    }

    /// Links a relocatable object into a fresh JITDylib and returns the function address,
    /// resource tracker, and guard for the JITDylib that owns the linked code.
    pub fn link_jit_object_in_fresh_dylib(
        &mut self,
        symbol_name: &CStr,
        object: &[u8],
        symbols: &[(CString, usize)],
    ) -> Result<(usize, orc::ResourceTracker, Arc<JitDylibGuard>)> {
        let orc = self.ensure_orc()?;
        let jd = orc.global.create_jit_dylib();
        let jd_guard = Arc::new(JitDylibGuard { global: orc.global, jd });
        let (addr, tracker) = link_jit_object_in_dylib(orc, jd, symbol_name, object, symbols)?;
        Ok((addr, tracker, jd_guard))
    }

    /// Pops and returns the [`ResourceTracker`](orc::ResourceTracker) for the last committed
    /// JIT module.
    ///
    /// Each call to [`jit_function`](revmc_backend::Backend::jit_function) commits a module
    /// with its own tracker. The caller takes ownership and is responsible for calling
    /// `tracker.remove()` to free the machine code when it is no longer needed.
    ///
    /// The caller must also hold a [`JitDylibGuard`] to prevent the owning JITDylib from
    /// being recycled before the tracker is removed.
    ///
    /// Returns `None` in AOT mode or if no modules have been committed.
    pub fn take_last_resource_tracker(&mut self) -> Option<orc::ResourceTracker> {
        let orc = self.orc.as_mut()?;
        let tracker = orc.loaded_trackers.pop()?;
        let tracker_idx = orc.loaded_trackers.len();
        orc.committed_functions.retain(|_, idx| *idx != tracker_idx);
        Some(tracker)
    }

    /// Returns a shared handle that keeps this backend's JITDylib alive.
    ///
    /// The JITDylib will not be cleared or recycled until all `JitDylibGuard` handles
    /// (and the backend itself) are dropped. Callers holding JIT function pointers must
    /// retain a guard to prevent the backing code from being freed.
    pub fn jit_dylib_guard(&self) -> Arc<JitDylibGuard> {
        Arc::clone(&self.orc.as_ref().expect("jit_dylib_guard called in AOT mode").jd_guard)
    }
}

impl BackendTypes for EvmLlvmBackend {
    type Type = BasicTypeEnum<'static>;
    type Value = BasicValueEnum<'static>;
    type StackSlot = PointerValue<'static>;
    type BasicBlock = BasicBlock<'static>;
    type Function = FunctionValue<'static>;
}

impl TypeMethods for EvmLlvmBackend {
    fn type_ptr(&self) -> Self::Type {
        self.ty_ptr.into()
    }

    fn type_ptr_sized_int(&self) -> Self::Type {
        self.ty_isize.into()
    }

    fn type_int(&self, bits: u32) -> Self::Type {
        match bits {
            1 => self.ty_i1,
            8 => self.ty_i8,
            16 => self.cx.i16_type(),
            32 => self.ty_i32,
            64 => self.ty_i64,
            128 => self.cx.i128_type(),
            256 => self.ty_i256,
            bits => self
                .cx
                .custom_width_int_type(std::num::NonZeroU32::new(bits).unwrap())
                .expect("custom int type"),
        }
        .into()
    }

    fn type_array(&self, ty: Self::Type, size: u32) -> Self::Type {
        ty.array_type(size).into()
    }

    fn type_bit_width(&self, ty: Self::Type) -> u32 {
        ty.into_int_type().get_bit_width()
    }
}

impl Backend for EvmLlvmBackend {
    type Builder<'a>
        = EvmLlvmBuilder<'a>
    where
        Self: 'a;
    type FuncId = u32;

    fn ir_extension(&self) -> &'static str {
        "ll"
    }

    fn set_module_name(&mut self, name: &str) {
        self.module().set_name(name);
    }

    fn config(&self) -> &BackendConfig {
        &self.backend_config
    }

    fn apply_config(&mut self, config: BackendConfig) {
        if self.backend_config.is_dumping != config.is_dumping {
            self.machine.set_asm_verbosity(config.is_dumping);
        }
        if self.backend_config.opt_level != config.opt_level {
            unsafe {
                cpp::revmc_llvm_target_machine_set_opt_level(
                    self.machine.as_mut_ptr(),
                    convert_opt_level(config.opt_level).into(),
                );
            }
        }
        self.backend_config = config;
    }

    fn finalize_debug_info(&mut self) -> Result<()> {
        if let Some(di) = &mut self.di_state
            && !di.finalized
        {
            di.dibuilder.finalize();
            di.finalized = true;
        }
        Ok(())
    }

    fn is_aot(&self) -> bool {
        self.aot
    }

    fn function_name_is_unique(&self, name: &str) -> bool {
        self.module().get_function(name).is_none()
    }

    fn dump_ir(&mut self, path: &Path) -> Result<()> {
        self.module().print_to_file(path).map_err(error_msg)
    }

    fn dump_disasm(&mut self, path: &Path) -> Result<()> {
        self.machine.write_to_file(self.module(), FileType::Assembly, path).map_err(error_msg)
    }

    fn build_function(
        &mut self,
        name: &str,
        ret: Option<Self::Type>,
        params: &[Self::Type],
        param_names: &[&str],
        linkage: revmc_backend::Linkage,
    ) -> Result<(Self::Builder<'_>, Self::FuncId)> {
        if !self.aot {
            self.ensure_orc()?;
        }
        let (id, function) = if let Some((&id, _fname)) =
            self.function_names.iter().find(|(_k, fname)| fname.as_str() == name)
            && let Some(orc) = &self.orc
            && let Some(function) = orc.staged_functions.get(&id).copied()
            && let Some(function2) = self.module().get_function(name)
            && function == function2
        {
            self.bcx.position_at_end(function.get_first_basic_block().unwrap());
            (id, function)
        } else {
            let fn_type = self.fn_type(ret, params);
            let function =
                self.module().add_function(name, fn_type, Some(convert_linkage(linkage)));
            cpp::set_dso_local(function);
            if self.backend_config.is_dumping {
                for (i, &name) in param_names.iter().enumerate() {
                    function.get_nth_param(i as u32).expect(name).set_name(self.name(name));
                }
            }

            let entry = self.cx.append_basic_block(function, self.name("entry"));
            self.bcx.position_at_end(entry);

            let id = self.function_counter;
            self.function_counter += 1;
            self.function_names.insert(id, name.to_string());
            if let Some(orc) = &mut self.orc {
                orc.staged_functions.insert(id, function);
            }
            (id, function)
        };

        // Attach debug info subprogram if debug is active.
        self.ensure_di_state();
        let debug_scope = if let Some(di) = &self.di_state {
            let file = di.compile_unit.get_file();
            let subroutine_type =
                di.dibuilder.create_subroutine_type(file, None, &[], DIFlags::PUBLIC);
            let subprogram = di.dibuilder.create_function(
                di.compile_unit.as_debug_info_scope(),
                name,
                None,
                file,
                0,
                subroutine_type,
                true,
                true,
                0,
                DIFlags::PUBLIC,
                self.backend_config.opt_level != OptimizationLevel::None,
            );
            function.set_subprogram(subprogram);
            Some(subprogram)
        } else {
            None
        };

        let builder = EvmLlvmBuilder { backend: self, function, debug_scope };
        Ok((builder, id))
    }

    fn verify_module(&mut self) -> Result<()> {
        self.module().verify().map_err(error_msg)
    }

    fn optimize_module(&mut self) -> Result<()> {
        // We use a custom pipeline instead of `default<O3>` because GVN is extremely slow on
        // the huge single-function modules that EVM compilation produces. Replacing GVN with
        // `early-cse` + `sccp` achieves equivalent or better code size and runtime performance
        // at ~5x faster compile time.
        //
        // The standard `default<O1>` is also slow (~730ms on snailtracer) because the loop
        // analysis infrastructure (LoopInfo, DominatorTree, MemorySSA, LCSSA) is expensive to
        // compute on functions with thousands of basic blocks, even though the loop passes
        // themselves do nothing useful — EVM has no natural loops to optimize.
        //
        // LICM (Loop Invariant Code Motion) helps tight EVM loops by hoisting gas counter and
        // stack slot loads/stores into registers. However, the loop analysis infrastructure is
        // quadratic on large functions — e.g. +430ms on snailtracer (7770 BBs) vs +0ms on
        // fibonacci (45 BBs). We skip it for functions with >4000 basic blocks.
        //
        // Can be overridden with `REVMC_PASSES` env var for experimentation.
        // From `opt --help`, `-passes`.

        static PASSES_OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        static PASSES: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        static PASSES_WITH_LICM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

        let passes = PASSES_OVERRIDE.get_or_init(|| std::env::var("REVMC_PASSES").ok());
        let passes = passes.as_deref().unwrap_or_else(|| match self.backend_config.opt_level {
            OptimizationLevel::None => "default<O0>",
            OptimizationLevel::Less | OptimizationLevel::Default => {
                let total_bbs: u32 =
                    self.module.get_functions().map(|f| f.count_basic_blocks()).sum();
                let with_licm = total_bbs <= 4000;
                let passes = if with_licm { &PASSES_WITH_LICM } else { &PASSES };
                passes.get_or_init(|| build_pass_pipeline(with_licm))
            }
            OptimizationLevel::Aggressive => "default<O3>",
        });
        let opts = PassBuilderOptions::create();
        self.module().run_passes(passes, &self.machine, opts).map_err(error_msg)
    }

    fn write_object<W: std::io::Write>(&mut self, mut w: W) -> Result<()> {
        let buffer = self
            .machine
            .write_to_memory_buffer(self.module(), FileType::Object)
            .map_err(error_msg)?;
        w.write_all(buffer.as_slice())?;
        Ok(())
    }

    fn jit_function(&mut self, id: Self::FuncId) -> Result<usize> {
        self.ensure_orc()?;
        self.commit_staged_module()?;
        let name_str = self.id_to_name(id);
        let name = CString::new(name_str).unwrap();
        let orc = self.orc.as_mut().unwrap();
        // Capture the compiled object buffer during lookup. LLJIT compiles lazily:
        // add_module_with_rt just registers the module, actual compilation happens
        // in lookup_in when the symbol is first requested.
        let mut captured = None;
        let _guard = ScopedObjCapture::install(&mut captured);
        let addr = orc.global.jit.lookup_in(orc.jd(), &name).map_err(error_msg)?;
        drop(_guard);
        if captured.is_some() {
            orc.last_compiled_object = captured;
        }
        Ok(addr)
    }

    fn function_name(&self, id: Self::FuncId) -> Option<&str> {
        self.function_names.get(&id).map(|s| s.as_str())
    }

    fn function_sizes(&self) -> Vec<(String, usize)> {
        let Some(orc) = &self.orc else { return Vec::new() };
        let Some(data) = orc.last_compiled_object.as_deref() else { return Vec::new() };
        let Ok(obj) = object::File::parse(data) else { return Vec::new() };

        let mut result: Vec<_> = obj
            .symbols()
            .filter(|sym| sym.is_definition())
            .filter_map(|sym| {
                let name = sym.name().ok()?;
                self.function_names.values().any(|n| n == name).then_some(())?;
                Some((name.to_string(), sym.size() as usize))
            })
            .collect();
        result.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
        result
    }

    fn clear_ir(&mut self) -> Result<()> {
        self.di_state = None;
        self.function_names.clear();
        self.module = create_module(self.cx, &self.machine, self.aot)?;
        if let Some(orc) = &mut self.orc {
            orc.staged_functions.clear();
            orc.pending_symbols.clear();
            orc.maybe_clear_dead_pool_entries();
        }
        Ok(())
    }

    unsafe fn free_function(&mut self, id: Self::FuncId) -> Result<()> {
        if let Some(orc) = &mut self.orc {
            // Remove from staging if not yet committed.
            if let Some(function) = orc.staged_functions.remove(&id) {
                unsafe { function.delete() };
            }

            // Remove from committed trackers.
            if let Some(tracker_idx) = orc.committed_functions.remove(&id) {
                orc.loaded_trackers[tracker_idx].remove().map_err(error_msg)?;
                // Also remove co-committed functions since removing the
                // tracker frees all symbols in that module.
                orc.committed_functions.retain(|co_id, idx| {
                    if *idx == tracker_idx {
                        self.function_names.remove(co_id);
                        false
                    } else {
                        true
                    }
                });
                // Note: the tracker slot becomes dead but indices of later
                // trackers are unchanged, keeping the map consistent.
            }
        }
        self.function_names.remove(&id);
        Ok(())
    }

    unsafe fn free_all_functions(&mut self) -> Result<()> {
        self.reset_jit()
    }
}

/// Cached target information for the host machine.
#[derive(Debug)]
struct TargetInfo {
    triple: TargetTriple,
    target: Target,
    cpu: String,
    features: String,
}

// SAFETY: No mutability is exposed and `TargetTriple` is an owned string.
unsafe impl std::marker::Send for TargetInfo {}
unsafe impl std::marker::Sync for TargetInfo {}

impl Clone for TargetInfo {
    fn clone(&self) -> Self {
        let triple = TargetTriple::create(self.triple.as_str().to_str().unwrap());
        Self {
            target: Target::from_triple(&triple).unwrap(),
            triple,
            cpu: self.cpu.clone(),
            features: self.features.clone(),
        }
    }
}

impl TargetInfo {
    fn new() -> &'static Self {
        static HOST_TARGET_INFO: OnceLock<TargetInfo> = OnceLock::new();
        HOST_TARGET_INFO.get_or_init(|| {
            let triple = TargetMachine::get_default_triple();
            let target = Target::from_triple(&triple).unwrap();
            let cpu = TargetMachine::get_host_cpu_name().to_string_lossy().into_owned();
            let features = TargetMachine::get_host_cpu_features().to_string_lossy().into_owned();
            Self { target, triple, cpu, features }
        })
    }
}

/// The LLVM-based EVM bytecode compiler function builder.
#[derive(Debug)]
#[must_use]
pub struct EvmLlvmBuilder<'a> {
    backend: &'a mut EvmLlvmBackend,
    function: FunctionValue<'static>,
    debug_scope: Option<DISubprogram<'static>>,
}

impl std::ops::Deref for EvmLlvmBuilder<'_> {
    type Target = EvmLlvmBackend;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.backend
    }
}

impl std::ops::DerefMut for EvmLlvmBuilder<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.backend
    }
}

impl EvmLlvmBuilder<'_> {
    #[allow(dead_code)]
    fn extract_value(
        &mut self,
        value: BasicValueEnum<'static>,
        index: u32,
        name: &str,
    ) -> BasicValueEnum<'static> {
        self.bcx.build_extract_value(value.into_struct_value(), index, self.name(name)).unwrap()
    }

    fn memcpy_inner(
        &mut self,
        dst: BasicValueEnum<'static>,
        src: BasicValueEnum<'static>,
        len: BasicValueEnum<'static>,
        inline: bool,
    ) {
        let dst = dst.into_pointer_value();
        let src = src.into_pointer_value();
        let len = len.into_int_value();
        let volatile = self.bool_const(false);
        let name = format!(
            "llvm.memcpy{}.p0.p0.{}",
            if inline { ".inline" } else { "" },
            fmt_ty(len.get_type().into()),
        );
        let memcpy = self.get_or_add_function(&name, |this| {
            this.ty_void.fn_type(
                &[this.ty_ptr.into(), this.ty_ptr.into(), this.ty_i64.into(), this.ty_i1.into()],
                false,
            )
        });
        self.bcx
            .build_call(memcpy, &[dst.into(), src.into(), len.into(), volatile.into()], "")
            .unwrap();
    }

    #[allow(dead_code)]
    fn call_overflow_function(
        &mut self,
        name: &str,
        lhs: BasicValueEnum<'static>,
        rhs: BasicValueEnum<'static>,
    ) -> (BasicValueEnum<'static>, BasicValueEnum<'static>) {
        let f = self.get_overflow_function(name, lhs.get_type());
        let result = self.call(f, &[lhs, rhs]).unwrap();
        (self.extract_value(result, 0, "result"), self.extract_value(result, 1, "overflow"))
    }

    #[allow(dead_code)]
    fn get_overflow_function(
        &mut self,
        name: &str,
        ty: BasicTypeEnum<'static>,
    ) -> FunctionValue<'static> {
        let name = format!("llvm.{name}.with.overflow.{}", fmt_ty(ty));
        self.get_or_add_function(&name, |this| {
            this.fn_type(
                Some(this.cx.struct_type(&[ty, this.ty_i1.into()], false).into()),
                &[ty, ty],
            )
        })
    }

    fn get_sat_function(
        &mut self,
        name: &str,
        ty: BasicTypeEnum<'static>,
    ) -> FunctionValue<'static> {
        let name = format!("llvm.{name}.sat.{}", fmt_ty(ty));
        self.get_or_add_function(&name, |this| this.fn_type(Some(ty), &[ty, ty]))
    }

    fn get_or_add_function(
        &mut self,
        name: &str,
        mk_ty: impl FnOnce(&mut Self) -> FunctionType<'static>,
    ) -> FunctionValue<'static> {
        match self.module().get_function(name) {
            Some(function) => function,
            None => {
                let ty = mk_ty(self);
                self.module().add_function(name, ty, None)
            }
        }
    }

    fn set_branch_weights(
        &self,
        inst: InstructionValue<'static>,
        weights: impl IntoIterator<Item = u32>,
    ) {
        let weights = weights.into_iter();
        let mut values = Vec::<BasicMetadataValueEnum<'static>>::with_capacity(
            1 + weights.size_hint().1.unwrap(),
        );
        values.push(self.cx.metadata_string("branch_weights").into());
        for weight in weights {
            values.push(self.ty_i32.const_int(weight as u64, false).into());
        }
        let metadata = self.cx.metadata_node(&values);
        let kind_id = self.cx.get_kind_id("prof");
        inst.set_metadata(metadata, kind_id).unwrap();
    }

    fn assume_inner(&mut self, cond: BasicValueEnum<'static>) -> CallSiteValue<'static> {
        let function = self.get_or_add_function("llvm.assume", |this| {
            this.ty_void.fn_type(&[this.ty_i1.into()], false)
        });
        self.bcx.build_call(function, &[cond.into()], "").unwrap()
    }
}

impl BackendTypes for EvmLlvmBuilder<'_> {
    type Type = <EvmLlvmBackend as BackendTypes>::Type;
    type Value = <EvmLlvmBackend as BackendTypes>::Value;
    type StackSlot = <EvmLlvmBackend as BackendTypes>::StackSlot;
    type BasicBlock = <EvmLlvmBackend as BackendTypes>::BasicBlock;
    type Function = <EvmLlvmBackend as BackendTypes>::Function;
}

impl TypeMethods for EvmLlvmBuilder<'_> {
    fn type_ptr(&self) -> Self::Type {
        self.backend.type_ptr()
    }

    fn type_ptr_sized_int(&self) -> Self::Type {
        self.backend.type_ptr_sized_int()
    }

    fn type_int(&self, bits: u32) -> Self::Type {
        self.backend.type_int(bits)
    }

    fn type_array(&self, ty: Self::Type, size: u32) -> Self::Type {
        self.backend.type_array(ty, size)
    }

    fn type_bit_width(&self, ty: Self::Type) -> u32 {
        self.backend.type_bit_width(ty)
    }
}

impl Builder for EvmLlvmBuilder<'_> {
    fn create_block(&mut self, name: &str) -> Self::BasicBlock {
        self.cx.append_basic_block(self.function, self.name(name))
    }

    fn create_block_after(&mut self, after: Self::BasicBlock, name: &str) -> Self::BasicBlock {
        self.cx.insert_basic_block_after(after, self.name(name))
    }

    fn switch_to_block(&mut self, block: Self::BasicBlock) {
        self.bcx.position_at_end(block);
    }

    fn seal_block(&mut self, block: Self::BasicBlock) {
        let _ = block;
        // Nothing to do.
    }

    fn seal_all_blocks(&mut self) {
        // Nothing to do.
    }

    fn set_current_block_cold(&mut self) {
        let true_ = self.bool_const(true);
        let callsite = self.assume_inner(true_);
        let cold = self.cx.create_enum_attribute(Attribute::get_named_enum_kind_id("cold"), 0);
        callsite.add_attribute(AttributeLoc::Function, cold);
    }

    fn current_block(&mut self) -> Option<Self::BasicBlock> {
        self.bcx.get_insert_block()
    }

    fn block_addr(&mut self, block: Self::BasicBlock) -> Option<Self::Value> {
        unsafe { block.get_address().map(Into::into) }
    }

    fn add_comment_to_current_inst(&mut self, comment: &str) {
        if !self.backend_config.is_dumping {
            return;
        }
        let Some(block) = self.current_block() else { return };
        let Some(ins) = block.get_last_instruction() else { return };
        let metadata = self.cx.metadata_string(comment);
        let metadata = self.cx.metadata_node(&[metadata.into()]);
        ins.set_metadata(metadata, self.cx.get_kind_id("annotation")).unwrap();
    }

    fn set_debug_location(&mut self, line: u32, col: u32) {
        let Some(scope) = self.debug_scope else { return };
        let Some(di) = &self.di_state else { return };
        let loc = di.dibuilder.create_debug_location(
            self.cx,
            line,
            col,
            scope.as_debug_info_scope(),
            None,
        );
        self.bcx.set_current_debug_location(loc);
    }

    fn clear_debug_location(&mut self) {
        if self.debug_scope.is_some() {
            self.bcx.unset_current_debug_location();
        }
    }

    fn fn_param(&mut self, index: usize) -> Self::Value {
        self.function.get_nth_param(index as _).unwrap()
    }

    fn num_fn_params(&self) -> usize {
        self.function.count_params() as usize
    }

    fn bool_const(&mut self, value: bool) -> Self::Value {
        self.ty_i1.const_int(value as u64, false).into()
    }

    fn iconst(&mut self, ty: Self::Type, value: i64) -> Self::Value {
        ty.into_int_type().const_int(value as u64, value.is_negative()).into()
    }

    fn uconst(&mut self, ty: Self::Type, value: u64) -> Self::Value {
        ty.into_int_type().const_int(value, false).into()
    }

    fn iconst_256(&mut self, value: impl TryInto<U256>) -> Self::Value {
        let value = value.try_into().ok().expect("invalid U256 value");
        if let Ok(low) = value.try_into() {
            return self.ty_i256.const_int(low, false).into();
        }

        self.scratch.clear();
        write!(self.scratch, "{value:x}").unwrap();
        self.ty_i256.const_int_from_string(&self.scratch, StringRadix::Hexadecimal).unwrap().into()
    }

    fn str_const(&mut self, value: &str) -> Self::Value {
        self.bcx.build_global_string_ptr(value, "").unwrap().as_pointer_value().into()
    }

    fn nullptr(&mut self) -> Self::Value {
        self.ty_ptr.const_null().into()
    }

    fn new_stack_slot_raw(&mut self, ty: Self::Type, name: &str) -> Self::StackSlot {
        // let ty = self.ty_i8.array_type(size);
        // let ptr = self.bcx.build_alloca(ty, self.name(name)).unwrap();
        // ptr.as_instruction().unwrap().set_alignment(align).unwrap();
        // ptr
        self.bcx.build_alloca(ty, self.name(name)).unwrap()
    }

    fn stack_load(&mut self, ty: Self::Type, slot: Self::StackSlot, name: &str) -> Self::Value {
        self.load(ty, slot.into(), name)
    }

    fn stack_store(&mut self, value: Self::Value, slot: Self::StackSlot) {
        self.store(value, slot.into())
    }

    fn stack_addr(&mut self, ty: Self::Type, slot: Self::StackSlot) -> Self::Value {
        let _ = ty;
        slot.into()
    }

    fn load(&mut self, ty: Self::Type, ptr: Self::Value, name: &str) -> Self::Value {
        let value = self.bcx.build_load(ty, ptr.into_pointer_value(), self.name(name)).unwrap();
        if ty == self.ty_i256.into() {
            self.current_block().unwrap().get_last_instruction().unwrap().set_alignment(8).unwrap();
        }
        value
    }

    fn load_aligned(
        &mut self,
        ty: Self::Type,
        ptr: Self::Value,
        align: usize,
        name: &str,
    ) -> Self::Value {
        let value = self.bcx.build_load(ty, ptr.into_pointer_value(), self.name(name)).unwrap();
        self.current_block()
            .unwrap()
            .get_last_instruction()
            .unwrap()
            .set_alignment(align as u32)
            .unwrap();
        value
    }

    fn store(&mut self, value: Self::Value, ptr: Self::Value) {
        let inst = self.bcx.build_store(ptr.into_pointer_value(), value).unwrap();
        if value.get_type() == self.ty_i256.into() {
            inst.set_alignment(8).unwrap();
        }
    }

    fn store_aligned(&mut self, value: Self::Value, ptr: Self::Value, align: usize) {
        let inst = self.bcx.build_store(ptr.into_pointer_value(), value).unwrap();
        inst.set_alignment(align as u32).unwrap();
    }

    fn nop(&mut self) {
        // LLVM doesn't have a NOP instruction.
    }

    fn ret(&mut self, values: &[Self::Value]) {
        match values {
            [] => self.bcx.build_return(None),
            [value] => self.bcx.build_return(Some(value)),
            values => self.bcx.build_aggregate_return(values),
        }
        .unwrap();
    }

    fn assume(&mut self, cond: Self::Value) {
        self.assume_inner(cond);
    }

    fn icmp(&mut self, cond: IntCC, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_int_compare(convert_intcc(cond), lhs.into_int_value(), rhs.into_int_value(), "")
            .unwrap()
            .into()
    }

    fn icmp_imm(&mut self, cond: IntCC, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.icmp(cond, lhs, rhs)
    }

    fn is_null(&mut self, ptr: Self::Value) -> Self::Value {
        self.bcx.build_is_null(ptr.into_pointer_value(), "").unwrap().into()
    }

    fn is_not_null(&mut self, ptr: Self::Value) -> Self::Value {
        self.bcx.build_is_not_null(ptr.into_pointer_value(), "").unwrap().into()
    }

    fn br(&mut self, dest: Self::BasicBlock) {
        self.bcx.build_unconditional_branch(dest).unwrap();
    }

    fn brif(
        &mut self,
        cond: Self::Value,
        then_block: Self::BasicBlock,
        else_block: Self::BasicBlock,
    ) {
        self.bcx.build_conditional_branch(cond.into_int_value(), then_block, else_block).unwrap();
    }

    fn brif_cold(
        &mut self,
        cond: Self::Value,
        then_block: Self::BasicBlock,
        else_block: Self::BasicBlock,
        then_is_cold: bool,
    ) {
        let inst = self
            .bcx
            .build_conditional_branch(cond.into_int_value(), then_block, else_block)
            .unwrap();
        let weights = if then_is_cold { [1, DEFAULT_WEIGHT] } else { [DEFAULT_WEIGHT, 1] };
        self.set_branch_weights(inst, weights);
    }

    fn switch(
        &mut self,
        index: Self::Value,
        default: Self::BasicBlock,
        targets: &[(u64, Self::BasicBlock)],
        default_is_cold: bool,
    ) {
        let ty = index.get_type().into_int_type();
        let targets =
            targets.iter().map(|(v, b)| (ty.const_int(*v, false), *b)).collect::<Vec<_>>();
        let inst = self.bcx.build_switch(index.into_int_value(), default, &targets).unwrap();
        if default_is_cold {
            let weights = iter::once(1).chain(iter::repeat_n(DEFAULT_WEIGHT, targets.len()));
            self.set_branch_weights(inst, weights);
        }
    }

    fn br_indirect(&mut self, address: Self::Value, destinations: &[Self::BasicBlock]) {
        let _ = self.bcx.build_indirect_branch(address, destinations).unwrap();
    }

    fn phi(&mut self, ty: Self::Type, incoming: &[(Self::Value, Self::BasicBlock)]) -> Self::Value {
        let incoming = incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue<'_>, *block))
            .collect::<Vec<_>>();
        let phi = self.bcx.build_phi(ty, "").unwrap();
        phi.add_incoming(&incoming);
        phi.as_basic_value()
    }

    fn select(
        &mut self,
        cond: Self::Value,
        then_value: Self::Value,
        else_value: Self::Value,
    ) -> Self::Value {
        self.bcx.build_select(cond.into_int_value(), then_value, else_value, "").unwrap()
    }

    fn iadd(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_int_add(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn isub(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_int_sub(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn imul(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_int_mul(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn udiv(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_int_unsigned_div(lhs.into_int_value(), rhs.into_int_value(), "")
            .unwrap()
            .into()
    }

    fn sdiv(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_int_signed_div(lhs.into_int_value(), rhs.into_int_value(), "")
            .unwrap()
            .into()
    }

    fn urem(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_int_unsigned_rem(lhs.into_int_value(), rhs.into_int_value(), "")
            .unwrap()
            .into()
    }

    fn srem(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_int_signed_rem(lhs.into_int_value(), rhs.into_int_value(), "")
            .unwrap()
            .into()
    }

    fn iadd_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.iadd(lhs, rhs)
    }

    fn isub_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.isub(lhs, rhs)
    }

    fn imul_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.imul(lhs, rhs)
    }

    // - [Avoid using arithmetic intrinsics](https://llvm.org/docs/Frontend/PerformanceTips.html)
    // - [Don't use usub.with.overflow intrinsic](https://github.com/rust-lang/rust/pull/103299)
    // - [for unsigned add overflow the recommended pattern is x + y < x](https://github.com/rust-lang/rust/pull/124114#issuecomment-2066173305)
    fn uadd_overflow(&mut self, lhs: Self::Value, rhs: Self::Value) -> (Self::Value, Self::Value) {
        let result = self.iadd(lhs, rhs);
        let overflow = self.icmp(IntCC::UnsignedLessThan, result, rhs);
        (result, overflow)
    }

    fn usub_overflow(&mut self, lhs: Self::Value, rhs: Self::Value) -> (Self::Value, Self::Value) {
        let result = self.isub(lhs, rhs);
        let overflow = self.icmp(IntCC::UnsignedLessThan, lhs, rhs);
        (result, overflow)
    }

    fn uadd_sat(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        let f = self.get_sat_function("uadd", lhs.get_type());
        self.call(f, &[lhs, rhs]).unwrap()
    }

    fn umax(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        let ty = lhs.get_type();
        let name = format!("llvm.umax.{}", fmt_ty(ty));
        let max = self.get_or_add_function(&name, |this| this.fn_type(Some(ty), &[ty, ty]));
        self.call(max, &[lhs, rhs]).unwrap()
    }

    fn umin(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        let ty = lhs.get_type();
        let name = format!("llvm.umin.{}", fmt_ty(ty));
        let min = self.get_or_add_function(&name, |this| this.fn_type(Some(ty), &[ty, ty]));
        self.call(min, &[lhs, rhs]).unwrap()
    }

    fn bswap(&mut self, value: Self::Value) -> Self::Value {
        let ty = value.get_type();
        let name = format!("llvm.bswap.{}", fmt_ty(ty));
        let bswap = self.get_or_add_function(&name, |this| this.fn_type(Some(ty), &[ty]));
        self.call(bswap, &[value]).unwrap()
    }

    fn bitor(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_or(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn bitand(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_and(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn bitxor(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_xor(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn bitnot(&mut self, value: Self::Value) -> Self::Value {
        self.bcx.build_not(value.into_int_value(), "").unwrap().into()
    }

    fn clz(&mut self, value: Self::Value) -> Self::Value {
        let ty = value.get_type();
        let i1_ty = self.type_int(1);
        let name = format!("llvm.ctlz.{}", fmt_ty(ty));
        let ctlz = self.get_or_add_function(&name, |this| this.fn_type(Some(ty), &[ty, i1_ty]));
        let is_poison_on_zero = self.bool_const(false);
        self.call(ctlz, &[value, is_poison_on_zero]).unwrap()
    }

    fn bitor_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.bitor(lhs, rhs)
    }

    fn bitand_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.bitand(lhs, rhs)
    }

    fn bitxor_imm(&mut self, lhs: Self::Value, rhs: i64) -> Self::Value {
        let rhs = self.iconst(lhs.get_type(), rhs);
        self.bitxor(lhs, rhs)
    }

    fn ishl(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx.build_left_shift(lhs.into_int_value(), rhs.into_int_value(), "").unwrap().into()
    }

    fn ushr(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_right_shift(lhs.into_int_value(), rhs.into_int_value(), false, "")
            .unwrap()
            .into()
    }

    fn sshr(&mut self, lhs: Self::Value, rhs: Self::Value) -> Self::Value {
        self.bcx
            .build_right_shift(lhs.into_int_value(), rhs.into_int_value(), true, "")
            .unwrap()
            .into()
    }

    fn zext(&mut self, ty: Self::Type, value: Self::Value) -> Self::Value {
        self.bcx.build_int_z_extend(value.into_int_value(), ty.into_int_type(), "").unwrap().into()
    }

    fn sext(&mut self, ty: Self::Type, value: Self::Value) -> Self::Value {
        self.bcx.build_int_s_extend(value.into_int_value(), ty.into_int_type(), "").unwrap().into()
    }

    fn ireduce(&mut self, to: Self::Type, value: Self::Value) -> Self::Value {
        self.bcx.build_int_truncate(value.into_int_value(), to.into_int_type(), "").unwrap().into()
    }

    fn inttoptr(&mut self, value: Self::Value, ty: Self::Type) -> Self::Value {
        self.bcx
            .build_int_to_ptr(value.into_int_value(), ty.into_pointer_type(), "")
            .unwrap()
            .into()
    }

    fn gep(
        &mut self,
        elem_ty: Self::Type,
        ptr: Self::Value,
        indexes: &[Self::Value],
        name: &str,
    ) -> Self::Value {
        let indexes = indexes.iter().map(|idx| idx.into_int_value()).collect::<Vec<_>>();
        unsafe {
            self.bcx.build_in_bounds_gep(
                elem_ty,
                ptr.into_pointer_value(),
                &indexes,
                self.name(name),
            )
        }
        .unwrap()
        .into()
    }

    fn tail_call(
        &mut self,
        function: Self::Function,
        args: &[Self::Value],
        tail_call: TailCallKind,
    ) -> Option<Self::Value> {
        let args = args.iter().copied().map(Into::into).collect::<Vec<_>>();
        let callsite = self.bcx.build_call(function, &args, "").unwrap();
        if let Some(call_conv) = function_call_conv(function) {
            unsafe {
                inkwell::llvm_sys::core::LLVMSetInstructionCallConv(
                    callsite.as_value_ref(),
                    call_conv as _,
                );
            }
        }
        if tail_call != TailCallKind::None {
            callsite.set_tail_call_kind(convert_tail_call_kind(tail_call));
        }
        callsite.try_as_basic_value().basic()
    }

    fn is_compile_time_known(&mut self, value: Self::Value) -> Option<Self::Value> {
        let ty = value.get_type();
        let name = format!("llvm.is.constant.{}", fmt_ty(ty));
        let f =
            self.get_or_add_function(&name, |this| this.fn_type(Some(this.ty_i1.into()), &[ty]));
        Some(self.call(f, &[value]).unwrap())
    }

    fn memcpy(&mut self, dst: Self::Value, src: Self::Value, len: Self::Value) {
        self.memcpy_inner(dst, src, len, false);
    }

    fn memcpy_inline(&mut self, dst: Self::Value, src: Self::Value, len: i64) {
        let len = self.iconst(self.ty_i64.into(), len);
        self.memcpy_inner(dst, src, len, true);
    }

    fn unreachable(&mut self) {
        self.bcx.build_unreachable().unwrap();
    }

    fn get_or_build_function(
        &mut self,
        name: &str,
        params: &[Self::Type],
        ret: Option<Self::Type>,
        linkage: revmc_backend::Linkage,
        build: impl FnOnce(&mut Self),
    ) -> Self::Function {
        if let Some(function) = self.module().get_function(name) {
            return function;
        }

        let before = self.current_block();

        let func_ty = self.fn_type(ret, params);
        let function = self.module().add_function(name, func_ty, Some(convert_linkage(linkage)));
        cpp::set_dso_local(function);
        let prev_function = std::mem::replace(&mut self.function, function);

        let entry = self.cx.append_basic_block(function, self.name("entry"));
        self.bcx.position_at_end(entry);
        build(self);
        if let Some(before) = before {
            self.bcx.position_at_end(before);
        }

        self.function = prev_function;

        function
    }

    fn get_function(&mut self, name: &str) -> Option<Self::Function> {
        self.module().get_function(name)
    }

    fn get_printf_function(&mut self) -> Self::Function {
        let name = "printf";
        if let Some(function) = self.module().get_function(name) {
            return function;
        }

        let ty = self.cx.void_type().fn_type(&[self.ty_ptr.into()], true);
        let function =
            self.module().add_function(name, ty, Some(inkwell::module::Linkage::External));
        cpp::set_dso_local(function);
        function
    }

    fn add_function(
        &mut self,
        name: &str,
        params: &[Self::Type],
        ret: Option<Self::Type>,
        address: Option<usize>,
        linkage: revmc_backend::Linkage,
        call_conv: CallConv,
    ) -> Self::Function {
        let func_ty = self.fn_type(ret, params);
        let function = self.module().add_function(name, func_ty, Some(convert_linkage(linkage)));
        set_function_call_conv(function, call_conv);
        cpp::set_dso_local(function);
        if let Some(address) = address
            && let Some(orc) = &mut self.orc
        {
            orc.pending_symbols.push((CString::new(name).unwrap(), address));
        }
        function
    }

    fn add_function_stub(
        &mut self,
        function: Self::Function,
        call_conv: CallConv,
    ) -> Self::Function {
        let name = function.get_name().to_string_lossy();
        let stub_name = format!("{name}.stub");
        if let Some(stub) = self.module().get_function(&stub_name) {
            return stub;
        }

        let func_ty = function.get_type();
        let stub = self.module().add_function(
            &stub_name,
            func_ty,
            Some(inkwell::module::Linkage::Internal),
        );
        set_function_call_conv(stub, call_conv);
        cpp::set_dso_local(stub);
        self.add_function_attribute(
            Some(stub),
            revmc_backend::Attribute::NoInline,
            revmc_backend::FunctionAttributeLocation::Function,
        );

        let before = self.bcx.get_insert_block();
        let debug_location = self.debug_scope.and_then(|_| self.bcx.get_current_debug_location());
        self.bcx.unset_current_debug_location();

        let entry = self.cx.append_basic_block(stub, self.name("stub"));
        self.bcx.position_at_end(entry);
        let args =
            (0..stub.count_params()).map(|i| stub.get_nth_param(i).unwrap()).collect::<Vec<_>>();
        if let Some(value) = self.call(function, &args) {
            self.bcx.build_return(Some(&value)).unwrap();
        } else {
            self.bcx.build_return(None).unwrap();
        }

        if let Some(before) = before {
            self.bcx.position_at_end(before);
        }
        if let Some(debug_location) = debug_location {
            self.bcx.set_current_debug_location(debug_location);
        }

        stub
    }

    fn add_function_attribute(
        &mut self,
        function: Option<Self::Function>,
        attribute: revmc_backend::Attribute,
        loc: revmc_backend::FunctionAttributeLocation,
    ) {
        let func = function.unwrap_or(self.function);
        let loc = convert_attribute_loc(loc);
        let attr = convert_attribute(self, attribute);
        func.add_attribute(loc, attr);
    }
}

/// Builds the LLVM pass pipeline string. See [`EvmLlvmBackend::optimize_module`].
fn build_pass_pipeline(with_licm: bool) -> String {
    let mut passes = String::from("module(globalopt,cgscc(inline),globalopt,function(");
    let function_passes: &[&str] = &[
        "simplifycfg",
        "sroa",
        "early-cse",
        "jump-threading",
        "correlated-propagation",
        "simplifycfg",
        "instcombine<no-verify-fixpoint>",
    ];
    let licm_passes: &[&str] =
        &["loop-mssa(licm,loop-rotate,licm)", "simplifycfg", "instcombine<no-verify-fixpoint>"];
    let post_passes: &[&str] = &[
        "sroa",
        "early-cse",
        "sccp",
        "instcombine<no-verify-fixpoint>",
        "adce",
        "dse",
        "simplifycfg",
    ];

    let iter =
        function_passes.iter().chain(if with_licm { licm_passes } else { &[] }).chain(post_passes);
    for (i, pass) in iter.enumerate() {
        if i > 0 {
            passes.push(',');
        }
        passes.push_str(pass);
    }
    passes.push_str("),globaldce)");
    passes
}

fn init() -> Result<()> {
    let mut init_result = Ok(());
    static INIT: Once = Once::new();
    INIT.call_once(|| init_result = init_());
    init_result
}

fn init_() -> Result<()> {
    // TODO: This also reports "PLEASE submit a bug report to..." when the segfault is
    // outside of LLVM.
    // enable_llvm_pretty_stack_trace();

    extern "C" fn report_fatal_error(msg: *const std::ffi::c_char) {
        let msg_cstr = unsafe { std::ffi::CStr::from_ptr(msg) };
        let msg = msg_cstr.to_string_lossy();
        error!(target: "llvm", "LLVM fatal error: {msg}");
    }

    unsafe {
        install_fatal_error_handler(report_fatal_error);
    }

    // Collect extra LLVM args from `REVMC_LLVM_ARGS` env var (space-separated).
    let extra: Vec<CString> = std::env::var("REVMC_LLVM_ARGS")
        .ok()
        .iter()
        .flat_map(|s| s.split_whitespace().filter_map(|s| CString::new(s).ok()).collect::<Vec<_>>())
        .collect();
    if !extra.is_empty() {
        debug!(extra = ?&extra, "passing extra LLVM args");
    }

    // The first arg is only used in `-help` output AFAICT.
    let mut args = vec![
        c"revmc-llvm".as_ptr(),
        c"-x86-asm-syntax=intel".as_ptr(),
        // CodeGenPrepare's memory optimization has a pathological interaction with our IR shape:
        // repeated stores to the same `gas_remaining` pointer in a huge function cause
        // MemoryDependenceResults::getNonLocalPointerDependency to do superlinear work.
        // Mark functions with ≥1000 BBs as "huge" to skip the expensive CGP memory opts,
        // which produce identical codegen at that scale. Small functions still benefit from
        // CGP's address sinking and branch optimizations.
        c"--cgpp-huge-func=1000".as_ptr(),
        // Greedy register allocation's global live range splitting is superlinear on large
        // functions with many long-lived values. Treat smaller live ranges as huge so LLVM uses
        // the cheaper splitting path; this preserves code size on snailtracer while cutting llc
        // time by ~3x.
        c"--huge-size-for-split=64".as_ptr(),
    ];
    args.extend(extra.iter().map(|s| s.as_ptr()));
    unsafe {
        inkwell::llvm_sys::support::LLVMParseCommandLineOptions(
            args.len() as i32,
            args.as_ptr(),
            std::ptr::null(),
        )
    }

    let config = InitializationConfig {
        asm_parser: false,
        asm_printer: true,
        base: true,
        disassembler: true,
        info: true,
        machine_code: true,
    };
    Target::initialize_native(&config).map_err(|e| eyre::eyre!("{e}"))?;

    Ok(())
}

fn create_module<'ctx>(
    cx: &'ctx Context,
    machine: &TargetMachine,
    aot: bool,
) -> Result<Module<'ctx>> {
    let module_name = "evm";
    let module = cx.create_module(module_name);
    module.set_source_file_name(module_name);
    module.set_data_layout(&machine.get_target_data().get_data_layout());
    module.set_triple(&machine.get_triple());
    if aot {
        module.add_basic_value_flag(
            "PIC Level",
            FlagBehavior::Error, // TODO: Min
            cx.i32_type().const_int(2, false),
        );
        module.add_basic_value_flag(
            "RtLibUseGOT",
            FlagBehavior::Warning,
            cx.i32_type().const_int(1, false),
        );
    }
    Ok(module)
}

fn convert_intcc(cond: IntCC) -> IntPredicate {
    match cond {
        IntCC::Equal => IntPredicate::EQ,
        IntCC::NotEqual => IntPredicate::NE,
        IntCC::SignedLessThan => IntPredicate::SLT,
        IntCC::SignedGreaterThanOrEqual => IntPredicate::SGE,
        IntCC::SignedGreaterThan => IntPredicate::SGT,
        IntCC::SignedLessThanOrEqual => IntPredicate::SLE,
        IntCC::UnsignedLessThan => IntPredicate::ULT,
        IntCC::UnsignedGreaterThanOrEqual => IntPredicate::UGE,
        IntCC::UnsignedGreaterThan => IntPredicate::UGT,
        IntCC::UnsignedLessThanOrEqual => IntPredicate::ULE,
    }
}

fn convert_opt_level(level: OptimizationLevel) -> inkwell::OptimizationLevel {
    match level {
        OptimizationLevel::None => inkwell::OptimizationLevel::None,
        OptimizationLevel::Less => inkwell::OptimizationLevel::Less,
        OptimizationLevel::Default => inkwell::OptimizationLevel::Default,
        OptimizationLevel::Aggressive => inkwell::OptimizationLevel::Aggressive,
    }
}

fn convert_attribute(bcx: &EvmLlvmBuilder<'_>, attr: revmc_backend::Attribute) -> Attribute {
    use revmc_backend::Attribute as OurAttr;

    enum AttrValue<'a> {
        String(&'a str),
        Enum(u64),
        Type(AnyTypeEnum<'a>),
    }

    let cpu;
    let (key, value) = match attr {
        OurAttr::WillReturn => ("willreturn", AttrValue::Enum(0)),
        OurAttr::NoReturn => ("noreturn", AttrValue::Enum(0)),
        OurAttr::NoFree => ("nofree", AttrValue::Enum(0)),
        OurAttr::NoRecurse => ("norecurse", AttrValue::Enum(0)),
        OurAttr::NoSync => ("nosync", AttrValue::Enum(0)),
        OurAttr::NoUnwind => ("nounwind", AttrValue::Enum(0)),
        OurAttr::NonLazyBind => ("nonlazybind", AttrValue::Enum(0)),
        OurAttr::UWTable => ("uwtable", AttrValue::Enum(2)),
        OurAttr::AllFramePointers => ("frame-pointer", AttrValue::String("all")),
        OurAttr::NativeTargetCpu => (
            "target-cpu",
            AttrValue::String({
                cpu = bcx.machine.get_cpu();
                cpu.to_str().unwrap()
            }),
        ),
        OurAttr::Cold => ("cold", AttrValue::Enum(0)),
        OurAttr::Hot => ("hot", AttrValue::Enum(0)),
        OurAttr::HintInline => ("inlinehint", AttrValue::Enum(0)),
        OurAttr::AlwaysInline => ("alwaysinline", AttrValue::Enum(0)),
        OurAttr::NoInline => ("noinline", AttrValue::Enum(0)),
        OurAttr::Speculatable => ("speculatable", AttrValue::Enum(0)),

        OurAttr::NoAlias => ("noalias", AttrValue::Enum(0)),
        OurAttr::NoCapture => ("captures", AttrValue::Enum(0)), // captures(none) - no capture
        OurAttr::NoUndef => ("noundef", AttrValue::Enum(0)),
        OurAttr::Align(n) => ("align", AttrValue::Enum(n)),
        OurAttr::NonNull => ("nonnull", AttrValue::Enum(0)),
        OurAttr::Dereferenceable(n) => ("dereferenceable", AttrValue::Enum(n)),
        OurAttr::SRet(n) => {
            ("sret", AttrValue::Type(bcx.type_array(bcx.ty_i8.into(), n as _).as_any_type_enum()))
        }
        OurAttr::ReadNone => ("readnone", AttrValue::Enum(0)),
        OurAttr::ReadOnly => ("readonly", AttrValue::Enum(0)),
        OurAttr::WriteOnly => ("writeonly", AttrValue::Enum(0)),
        OurAttr::Writable => ("writable", AttrValue::Enum(0)),
        // memory(argmem: readwrite) = ModRef(3) << ArgMem(0) = 3.
        OurAttr::ArgMemOnly => ("memory", AttrValue::Enum(3)),
        OurAttr::DeadOnReturn => ("dead_on_return", AttrValue::Enum(0)),

        OurAttr::Initializes(size) => {
            return cpp::create_initializes_attr(bcx.cx, 0, size as i64);
        }

        attr => unimplemented!("llvm attribute conversion: {attr:?}"),
    };
    match value {
        AttrValue::String(value) => bcx.cx.create_string_attribute(key, value),
        AttrValue::Enum(value) => {
            let id = Attribute::get_named_enum_kind_id(key);
            bcx.cx.create_enum_attribute(id, value)
        }
        AttrValue::Type(ty) => {
            let id = Attribute::get_named_enum_kind_id(key);
            bcx.cx.create_type_attribute(id, ty)
        }
    }
}

fn convert_attribute_loc(loc: revmc_backend::FunctionAttributeLocation) -> AttributeLoc {
    match loc {
        revmc_backend::FunctionAttributeLocation::Return => AttributeLoc::Return,
        revmc_backend::FunctionAttributeLocation::Param(i) => AttributeLoc::Param(i),
        revmc_backend::FunctionAttributeLocation::Function => AttributeLoc::Function,
    }
}

fn function_call_conv(function: FunctionValue<'_>) -> Option<u32> {
    let call_conv =
        unsafe { inkwell::llvm_sys::core::LLVMGetFunctionCallConv(function.as_value_ref()) };
    (call_conv != inkwell::llvm_sys::LLVMCallConv::LLVMCCallConv as u32).then_some(call_conv)
}

fn set_function_call_conv(function: FunctionValue<'_>, call_conv: CallConv) {
    let Some(call_conv) = convert_call_conv(call_conv) else { return };
    unsafe {
        inkwell::llvm_sys::core::LLVMSetFunctionCallConv(function.as_value_ref(), call_conv as _);
    }
}

fn convert_call_conv(call_conv: CallConv) -> Option<inkwell::llvm_sys::LLVMCallConv> {
    match call_conv {
        CallConv::Default => None,
        CallConv::Cold => Some(inkwell::llvm_sys::LLVMCallConv::LLVMPreserveMostCallConv),
    }
}

fn convert_linkage(linkage: revmc_backend::Linkage) -> inkwell::module::Linkage {
    match linkage {
        revmc_backend::Linkage::Public => inkwell::module::Linkage::External,
        revmc_backend::Linkage::Import => inkwell::module::Linkage::External,
        revmc_backend::Linkage::Private => inkwell::module::Linkage::Private,
    }
}

fn convert_tail_call_kind(kind: TailCallKind) -> inkwell::llvm_sys::LLVMTailCallKind {
    match kind {
        TailCallKind::None => inkwell::llvm_sys::LLVMTailCallKind::LLVMTailCallKindNone,
        TailCallKind::Tail => inkwell::llvm_sys::LLVMTailCallKind::LLVMTailCallKindTail,
        TailCallKind::MustTail => inkwell::llvm_sys::LLVMTailCallKind::LLVMTailCallKindMustTail,
        TailCallKind::NoTail => inkwell::llvm_sys::LLVMTailCallKind::LLVMTailCallKindNoTail,
    }
}

// No `#[track_caller]` because `map_err` doesn't propagate it.
fn error_msg(msg: inkwell::support::LLVMString) -> revmc_backend::Error {
    revmc_backend::Error::msg(msg.to_string_lossy().trim_end().to_string())
}

fn fmt_ty(ty: BasicTypeEnum<'_>) -> impl std::fmt::Display {
    ty.print_to_string().to_str().unwrap().trim_matches('"').to_string()
}
